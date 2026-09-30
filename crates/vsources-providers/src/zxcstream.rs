//! `ZXCStream`: the token-protocol rewrite over `player.zxcstream.xyz`.
//!
//! Ports `src/source/ZXCStream.js` + its Nuvio scraper
//! `src/nuvio/zxcstream.cjs` (Task 64/71/86 protocol: the site's
//! backend migrated to obfuscated request fields, a rotating token
//! route, and the fingerprint-gated, PoW-protected, AES-GCM-encrypted
//! "Byse" playback API on `mfw09.org`).
//!
//! Flow (the scraper's stages, verbatim):
//!
//! 1. Resolve the TMDB id, name/year, and best-effort `IMDb` id (the
//!    context media or [`TmdbClient`]) — the scraper's own TMDB
//!    details fetch.
//! 2. **Stage A — the player token**: `fToken` =
//!    `sha512("{ts}:{SECRET}:{tmdbId}")[0..64]` (`SECRET` is the
//!    chunk constant "23423653"); `POST /backend/{route}` with the
//!    obfuscated `FIELD_MAP` body (`id`, `fToken`, `ts`, `path`,
//!    `mediaType` — the backend requires `path` and `mediaType` even
//!    though the site's own frontend omits them), trying the token
//!    routes in order (`ololmo`, `burat`, `bugok`, `abaygagoka` — the
//!    route has rotated 5× and the old names sometimes come back)
//!    over both player bases (`zxcprime` first, `zxcstream` second).
//!    `GET /backend_/embed/sentinel` with the obfuscated query then
//!    returns the `mfw09.org/e/{code}` embed URL.
//! 3. **Stage B — Byse attestation**: a fresh P-256 identity
//!    (`POST /api/videos/access/challenge` → sign the nonce →
//!    `POST /api/videos/access/attest` with the raw `r‖s` signature,
//!    the JWK public key, and the fixed browser fingerprint) yields
//!    the `{viewer_id, device_id, confidence}` triple, cached 45 min.
//! 4. **Stage C — proof of work (`PoW`)**: `POST …/embed/captcha` →
//!    `{pow_nonce, pow_difficulty, pow_token}`; the `PoW` is the site's
//!    custom 32-word hash (`byse_hash`, an exact port of
//!    `pow-DEJGtdh2.js`'s `ye/gr/wr` functions): find the counter
//!    where `byse_hash("{nonce}:{counter}")` has ≥ difficulty
//!    leading zero bits; `POST …/embed/captcha/verify` with the
//!    solution returns the captcha token.
//! 5. **Stage D — playback**: `POST …/embed/playback` with the
//!    `X-Captcha-Token` header and the `snake_case` attestation
//!    fingerprint → `{playback: {version, key_parts, iv, payload}}`;
//!    the AES-256-GCM key is `key_parts[version-1] ‖
//!    key_parts[30-version]` (16-byte base64url parts), the tag is
//!    the payload's last 16 bytes, and the plaintext JSON is
//!    `{sources, tracks, poster_url}`.
//! 6. **Stage E — subtitles**: the sentinel embed's `sub.info` query
//!    URL serves the track list (`{file, label}` entries).
//! 7. Only extracted media URLs ship — the old player-page fallback
//!    card is gone (Task 55: mpv cannot play HTML), and every
//!    protocol miss is an honest zero → [`SourceError::NotFound`].
//!    The card: `ZXCStream - {quality}` name, the
//!    `name [S01E01] (year)` title, the
//!    `User-Agent`/`Referer: https://mfw09.org/` hotlink headers, and
//!    the `zxcstream-hls`/`zxcstream-mp4` binge groups.
//!
//! Cuts for the library port:
//!
//! - The signatures: the webcrypto path produces raw IEEE-P1363
//!   `r‖s`; the node fallback produces DER — the port always emits
//!   the raw layout (the browser path the site's own bundle uses).
//!   The nonce is RFC 6979 deterministic (upstream's is random);
//!   any valid nonce satisfies the server.
//! - The ECDSA layer is a minimal P-256 implementation over
//!   `num-bigint` (the workspace has no ECDSA crate); it reproduces
//!   the RFC 6979 A.2.5 test vectors and OpenSSL-generated public
//!   keys byte-for-byte (see the tests).
//! - Upstream races the whole call at 25 s (`callNuvioProvider`'s
//!   `timeoutMs`) — this port wraps the protocol in
//!   [`with_deadline`].
//! - The `sub.info` fetch went through `got-scraping` (h2, then h1,
//!   with a 900 ms cross-Cloudflare pause); the shared fetcher's
//!   browser emulation replaces it, keeping the two-attempt shape.
//! - Upstream's `Referer` on the cards routes the media through the
//!   Nuvio `/proxy` (the sprintcdn tokens are bound to the minting
//!   IP — live-measured 200 from it, 404 elsewhere); there is no
//!   proxy here, so the headers ride
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta)
//!   verbatim and the IP binding is an honest limitation.
//! - `meta.title` has no `StreamMeta` field — the card title rides
//!   [`Stream::label`] via `build_stream_results`.

mod current;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256, Sha512};
use tokio::sync::Mutex;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline};

/// The player the source labels (upstream `this.baseUrl`).
const BASE_URL: &str = "https://player.zxcstream.xyz";
/// Upstream `this.ttl` — 5 min (tokens are time-limited).
const TTL: Duration = Duration::from_mins(5);
/// Upstream `callNuvioProvider`'s `timeoutMs` — the whole-protocol race.
const DEADLINE: Duration = Duration::from_secs(25);
/// Upstream `fetchRaw`'s default timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// The playback request timeout (upstream uses 15 s there).
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(15);
/// The `PoW` solve budget (upstream `solveBysePoW(…, 15000)`).
const POW_BUDGET: Duration = Duration::from_secs(15);
/// The attestation cache window — upstream's 45 minutes.
const ATTEST_TTL: Duration = Duration::from_mins(45);
/// The browser `User-Agent` the scraper sends (Chrome 147).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

/// The token-route `SECRET` — the deployed chunk constant (unchanged
/// across every route rotation).
const SECRET: &str = "23423653";

/// The obfuscated `FIELD_MAP` — the request/response field names
/// verbatim from the deployed chunk (module 55790).
mod field_map {
    /// The `id` field.
    pub const ID: &str = "a7f39c821d604e5b9c7143f36e1547b";
    /// The `fToken` field.
    pub const FTOKEN: &str = "e83c4b719a52d8f3136052479c1635a";
    /// The `ts` field.
    pub const TS: &str = "61d9a5274c8e3b29af75d6384c291e6";
    /// The `token` field.
    pub const TOKEN: &str = "c492f7a183d6502b1e7436c538a716d";
    /// The `season` field.
    pub const SEASON: &str = "d8427b59ce30684a2f957c3613e85b";
    /// The `episode` field.
    pub const EPISODE: &str = "91c6e4a728bd503d1f785c92346b713d";
    /// The `imdbId` field.
    pub const IMDB_ID: &str = "f35a8c19d674b3265e871c4933a725f";
    /// The `path` field.
    pub const PATH: &str = "6b491e7253ad8f14d392e7561a9384c";
    /// The `mediaType` field.
    pub const MEDIA_TYPE: &str = "c285f91ab306d281e947a35632e816b";
}

/// The player bases — `zxcprime` is the live 302 target, `zxcstream`
/// the old origin (upstream `PLAYER_BASES`, in order).
const PLAYER_BASES: [&str; 2] = [
    "https://player.zxcprime.xyz",
    "https://player.zxcstream.xyz",
];

/// The token routes, newest first (upstream `TOKEN_ROUTES` — the
/// route has rotated five times; the old names sometimes come back
/// behind the origin proxy).
const TOKEN_ROUTES: [&str; 4] = [
    "/backend/ololmo",
    "/backend/burat",
    "/backend/bugok",
    "/backend/abaygagoka",
];

/// The `ZXCStream` provider.
pub struct ZXCStream {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The cached Byse attestation (upstream's module-level
    /// `_attest` — shared across resolves, 45-minute window).
    attest: Mutex<Option<Identity>>,
}

impl ZXCStream {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "zxcstream".to_string(),
                label: "ZXCStream".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            attest: Mutex::new(None),
        }
    }

    /// The whole scraper pipeline — stage A through E, the card fan,
    /// and the binge-group pass.
    async fn protocol(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        imdb_id: Option<String>,
        name: &str,
        year: Option<u16>,
    ) -> Vec<Stream> {
        // The deployed player now exposes a direct multi-server API. Keep
        // the historical Byse route as fallback for older mirrors.
        let current = with_deadline(
            current::resolve(ctx, media, tmdb_id, name, year),
            Duration::from_secs(8),
        )
        .await
        .unwrap_or_default();
        if !current.is_empty() {
            return current;
        }
        let kind = kind_of(media);
        let Some(embed) = resolve_embed(
            ctx,
            tmdb_id,
            kind,
            media.season,
            media.episode,
            imdb_id.as_deref(),
        )
        .await
        else {
            return Vec::new();
        };
        let Some(identity) = self.attestation(ctx).await else {
            return Vec::new();
        };
        let Some(config) = resolve_byse_sources(ctx, &embed, &identity).await else {
            return Vec::new();
        };
        let subtitles = subtitle_tracks(ctx, &embed).await;

        let Some(sources) = config.get("sources").and_then(Value::as_array) else {
            return Vec::new();
        };
        let title = title_line(name, year, media);
        let mut raw: Vec<NuvioStream> = Vec::new();
        for source in sources {
            let Some(stream) = card(source, &title, &subtitles) else {
                continue;
            };
            raw.push(stream);
        }
        if raw.is_empty() {
            return Vec::new();
        }

        let params = BuildParams {
            streams: &raw,
            title: &base_title(name, year, media),
            source_id: &self.info.id,
            source_label: &self.info.label,
            country_codes: &self.info.country_codes,
            ttl: TTL,
        };
        let mut streams = build_stream_results(&params);
        // The JS attaches `behaviorHints.bingeGroup` per card; the
        // shared builder does not carry it, so it lands here.
        for stream in &mut streams {
            let group = if stream.format == Format::Hls {
                "zxcstream-hls"
            } else {
                "zxcstream-mp4"
            };
            stream
                .behavior_hints
                .insert("bingeGroup".to_string(), group.to_string());
        }
        streams
    }

    /// Stage B — the Byse attestation, with the 45-minute cache.
    async fn attestation(&self, ctx: &ResolveCtx<'_>) -> Option<Identity> {
        {
            let cached = self.attest.lock().await;
            if let Some(identity) = cached.as_ref()
                && identity.expires_at > Instant::now()
            {
                return Some(identity.clone());
            }
        }

        let key = p256::PrivateKey::generate();
        let challenge = post_json(
            ctx,
            "https://mfw09.org/api/videos/access/challenge",
            &chrome_headers(),
            "{}",
            REQUEST_TIMEOUT,
        )
        .await?;
        let challenge_id = str_field(&challenge, "challenge_id")?.to_string();
        let nonce = str_field(&challenge, "nonce")?.to_string();

        let signature = b64url(&key.sign(nonce.as_bytes()));
        let body = json!({
            "viewer_id": "",
            "device_id": "",
            "challenge_id": challenge_id,
            "nonce": nonce,
            "signature": signature,
            "public_key": key.public_key().jwk(),
            "client": browser_fingerprint(),
            "storage": {},
            "attributes": { "entropy": "medium" },
        });
        let attested = post_json(
            ctx,
            "https://mfw09.org/api/videos/access/attest",
            &chrome_headers(),
            &body.to_string(),
            REQUEST_TIMEOUT,
        )
        .await?;
        let device_id = str_field(&attested, "device_id")?.to_string();
        let identity = Identity {
            viewer_id: str_field(&attested, "viewer_id")
                .unwrap_or_default()
                .to_string(),
            device_id,
            confidence: attested
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.5),
            expires_at: Instant::now() + ATTEST_TTL,
        };
        *self.attest.lock().await = Some(identity.clone());
        Some(identity)
    }
}

#[async_trait]
impl Source for ZXCStream {
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
            self.protocol(ctx, media, tmdb_id, imdb_id, &name, year),
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

/// The Byse identity — stage B's output.
#[derive(Clone)]
struct Identity {
    /// The attested viewer id.
    viewer_id: String,
    /// The attested device id.
    device_id: String,
    /// The server's confidence score.
    confidence: f64,
    /// When the identity expires.
    expires_at: Instant,
}

// -- stage A: the player token and sentinel -------------------------------

/// The frontend token pair — `fToken` and its timestamp.
struct FrontendToken {
    /// `sha512("{ts}:{SECRET}:{tmdbId}")[0..64]` as hex.
    xt: String,
    /// The token's millisecond timestamp.
    rt: u64,
}

/// `generateFrontendToken` — the hex `fToken` over the secret.
fn generate_frontend_token(tmdb_id: u64) -> FrontendToken {
    let ts = now_millis();
    let input = format!("{ts}:{SECRET}:{tmdb_id}");
    let digest = Sha512::digest(input.as_bytes());
    let hex = hex_string(&digest);
    FrontendToken {
        xt: hex[..64].to_string(),
        rt: ts,
    }
}

/// Stage A — the token POST over the routes and bases, then the
/// sentinel embed. Returns the embed URL.
async fn resolve_embed(
    ctx: &ResolveCtx<'_>,
    tmdb_id: u64,
    kind: &str,
    season: Option<u32>,
    episode: Option<u32>,
    imdb_id: Option<&str>,
) -> Option<String> {
    let id_str = tmdb_id.to_string();
    let page_path = format!("/embed/{kind}/{id_str}");
    let token = generate_frontend_token(tmdb_id);

    // The obfuscated body — `path` and `mediaType` are required even
    // though the site's own frontend omits them (400 without).
    let mut body = Map::new();
    body.insert(field_map::ID.to_string(), Value::String(id_str.clone()));
    body.insert(
        field_map::FTOKEN.to_string(),
        Value::String(token.xt.clone()),
    );
    body.insert(
        field_map::TS.to_string(),
        Value::String(token.rt.to_string()),
    );
    body.insert(
        field_map::PATH.to_string(),
        Value::String(format!("{}{page_path}", PLAYER_BASES[0])),
    );
    body.insert(
        field_map::MEDIA_TYPE.to_string(),
        Value::String(kind.to_string()),
    );
    let body = Value::Object(body).to_string();

    for base in PLAYER_BASES {
        // The rotating token route: first 200 wins.
        let mut token_json: Option<Value> = None;
        for route in TOKEN_ROUTES {
            let Ok(url) = Url::parse(&format!("{base}{route}")) else {
                continue;
            };
            let request = FetchRequest::post(url, body.clone())
                .with_header("Content-Type", "application/json")
                .with_header("Accept", "application/json, text/plain, */*")
                .with_header("Origin", base)
                .with_header("Referer", format!("{base}{page_path}"))
                .with_timeout(REQUEST_TIMEOUT);
            let Ok(response) = ctx.fetcher.request(request).await else {
                continue;
            };
            if response.status != 200 {
                continue;
            }
            token_json = serde_json::from_str(&response.body).ok();
            break;
        }
        let Some(json) = token_json else {
            continue;
        };
        // The field names come back obfuscated too (with plain
        // fallbacks).
        let token_value = str_field(&json, field_map::TOKEN)
            .or_else(|| str_field(&json, "token"))
            .map(str::to_string);
        let server_ts = str_field(&json, field_map::TS)
            .or_else(|| str_field(&json, "ts"))
            .map(str::to_string);
        let (Some(token_value), Some(server_ts)) = (token_value, server_ts) else {
            continue;
        };

        // The sentinel query — obfuscated keys, `b` in the clear.
        let mut url = Url::parse(&format!("{base}/backend_/embed/sentinel")).ok()?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair(field_map::ID, &id_str);
            pairs.append_pair("b", kind);
            pairs.append_pair(field_map::TS, &server_ts);
            pairs.append_pair(field_map::TOKEN, &token_value);
            pairs.append_pair(field_map::FTOKEN, &token.xt);
            if kind == "tv" && season.is_some() {
                pairs.append_pair(field_map::SEASON, &season.unwrap_or(1).to_string());
                pairs.append_pair(field_map::EPISODE, &episode.unwrap_or(1).to_string());
            }
            if let Some(imdb_id) = imdb_id {
                pairs.append_pair(field_map::IMDB_ID, imdb_id);
            }
        }
        let request = FetchRequest::get(url)
            .with_header("Referer", format!("{base}{page_path}"))
            .with_timeout(REQUEST_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            continue;
        };
        if response.status != 200 {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(&response.body) else {
            continue;
        };
        if let Some(embed) = str_field(&json, "embed")
            && embed.starts_with("https://")
        {
            return Some(embed.to_string());
        }
    }
    None
}

// -- stage C: the custom PoW hash ------------------------------------------

/// The four-word mixing round — `mix4` from the deployed chunk.
fn mix4(state: &mut [u32; 4]) {
    state[0] = state[0].wrapping_add(state[1]);
    state[3] = (state[3] ^ state[0]).rotate_left(16);
    state[2] = state[2].wrapping_add(state[3]);
    state[1] = (state[1] ^ state[2]).rotate_left(12);
    state[0] = state[0].wrapping_add(state[1]);
    state[3] = (state[3] ^ state[0]).rotate_left(8);
    state[2] = state[2].wrapping_add(state[3]);
    state[1] = (state[1] ^ state[2]).rotate_left(7);
}

/// The custom 32-word `PoW` hash — an exact port of the chunk's
/// `byseHash` (the SHA-512-IV-seeded state, the 512-entry mixing
/// table, and the 8×64 fold).
fn byse_hash(bytes: &[u8]) -> [u32; 8] {
    let mut state: [u32; 4] = [1_779_033_703, 3_144_134_277, 1_013_904_242, 2_773_480_762];
    for &byte in bytes {
        state[0] = state[0].wrapping_add(u32::from(byte));
        state[0] = state[0].rotate_left(7);
        mix4(&mut state);
    }
    for _ in 0..8 {
        mix4(&mut state);
    }
    let mut table = [0u32; 512];
    for slot in &mut table {
        mix4(&mut state);
        *slot = state[0] ^ state[2];
    }
    for _ in 0..2 {
        for index in 0..512 {
            let mixed = (table[index] & 511) as usize;
            let mut value = table[index].wrapping_add(table[mixed]);
            value = value.rotate_left(13);
            value ^= table[(index + 1) & 511].wrapping_mul(2_654_435_761);
            table[index] = value;
            state[0] ^= value;
            mix4(&mut state);
        }
    }
    let mut out = [0u32; 8];
    for (word, slot) in out.iter_mut().enumerate() {
        mix4(&mut state);
        let mut accumulator = state[0];
        let base = word * 64;
        for offset in 0..64 {
            let value = table[base + offset];
            accumulator = accumulator.wrapping_add(value);
            accumulator = accumulator.rotate_left(5);
            accumulator ^= value.wrapping_mul(2_246_822_519);
        }
        *slot = accumulator ^ state[2];
    }
    out
}

/// `leadingZeroBits` — the `clz32` ladder over the words.
fn leading_zero_bits(words: &[u32; 8]) -> u32 {
    let mut bits = 0;
    for &word in words {
        if word == 0 {
            bits += 32;
            continue;
        }
        return bits + word.leading_zeros();
    }
    bits
}

/// `solveBysePoW` — the counter search under the time budget. The
/// counter renders as `{nonce}:{counter}` (ASCII, like the JS's
/// `charCodeAt & 255` bytes).
fn solve_byse_pow(nonce: &str, difficulty: u32, budget: Duration) -> Option<String> {
    if difficulty == 0 {
        return Some("0".to_string());
    }
    let started = Instant::now();
    let mut counter: u64 = 0;
    loop {
        let message = format!("{nonce}:{counter}");
        if leading_zero_bits(&byse_hash(message.as_bytes())) >= difficulty {
            return Some(counter.to_string());
        }
        counter += 1;
        if counter.is_multiple_of(1024) && started.elapsed() > budget {
            return None;
        }
    }
}

// -- stages D and E: playback and subtitles --------------------------------

/// Stage D — the captcha challenge, the `PoW` solve, the verify, and
/// the GCM playback config.
async fn resolve_byse_sources(
    ctx: &ResolveCtx<'_>,
    embed_url: &str,
    identity: &Identity,
) -> Option<Value> {
    let code = embed_code(embed_url)?;

    // 5. The captcha challenge.
    let challenge = post_json(
        ctx,
        &format!("https://mfw09.org/api/videos/{code}/embed/captcha"),
        &chrome_headers(),
        "{}",
        REQUEST_TIMEOUT,
    )
    .await?;
    let pow_nonce = str_field(&challenge, "pow_nonce")?.to_string();
    let pow_token = str_field(&challenge, "pow_token")?.to_string();
    let difficulty = challenge
        .get("pow_difficulty")
        .and_then(Value::as_u64)
        .unwrap_or(16)
        .try_into()
        .unwrap_or(16);

    // 6. Solve and verify.
    let solution = solve_byse_pow(&pow_nonce, difficulty, POW_BUDGET)?;
    let verify_body = json!({ "pow_token": pow_token, "solution": solution });
    let verified = post_json(
        ctx,
        &format!("https://mfw09.org/api/videos/{code}/embed/captcha/verify"),
        &chrome_headers(),
        &verify_body.to_string(),
        REQUEST_TIMEOUT,
    )
    .await?;
    if str_field(&verified, "status").unwrap_or_default() != "ok" {
        return None;
    }
    let captcha_token = str_field(&verified, "token")?.to_string();

    // 7. The playback config (the fingerprint body is snake_case).
    let body = json!({
        "fingerprint": {
            "viewer_id": identity.viewer_id,
            "device_id": identity.device_id,
            "confidence": identity.confidence,
        }
    });
    let mut headers: Vec<(&str, &str)> = chrome_headers().to_vec();
    headers.push(("X-Captcha-Token", captcha_token.as_str()));
    let playback = post_json(
        ctx,
        &format!("https://mfw09.org/api/videos/{code}/embed/playback"),
        &headers,
        &body.to_string(),
        PLAYBACK_TIMEOUT,
    )
    .await?;
    let config = playback.get("playback")?;

    // 8. AES-256-GCM — the key-parts pair, the tag as the payload's
    //    last 16 bytes.
    let key = assemble_key(config)?;
    let iv = b64u_decode(str_field(config, "iv")?)?;
    let payload = b64u_decode(str_field(config, "payload")?)?;
    if payload.len() < 16 || iv.len() != 12 {
        return None;
    }
    let (ciphertext, tag) = payload.split_at(payload.len() - 16);
    let plain = gcm::decrypt(&key, &iv, tag, ciphertext)?;
    serde_json::from_slice(&plain).ok()
}

/// The embed's video code — the last path segment of the sentinel
/// embed URL.
fn embed_code(embed_url: &str) -> Option<String> {
    Url::parse(embed_url)
        .ok()?
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|code| !code.is_empty())
        .map(str::to_string)
}

/// The AES-256-GCM key — `key_parts[version-1] ‖ key_parts[30-version]`
/// (16-byte base64url parts, 1-indexed pair from the site's table),
/// falling back to the whole table, 32 bytes either way.
fn assemble_key(playback: &Value) -> Option<Vec<u8>> {
    let parts = playback.get("key_parts")?.as_array()?;
    let version = playback.get("version")?.as_u64()?;
    let part = |index: i64| -> Option<Vec<u8>> {
        let index = usize::try_from(index).ok()?;
        b64u_decode(parts.get(index)?.as_str()?)
    };
    let version = i64::try_from(version).ok()?;
    let first = part(version - 1);
    let second = part(30 - version);
    let key = match (first, second) {
        (Some(mut head), Some(tail)) => {
            head.extend_from_slice(&tail);
            head
        }
        // The whole-table fallback (the JS concatenates every part
        // when the pair is missing).
        _ => parts
            .iter()
            .filter_map(|value| value.as_str().and_then(b64u_decode))
            .flatten()
            .collect(),
    };
    if key.len() == 32 { Some(key) } else { None }
}

/// Stage E — the `sub.info` track list. Two attempts with the 900 ms
/// cross-Cloudflare pause (upstream tried h2 then h1 through
/// `got-scraping`; the shared fetcher replaces both transports).
async fn subtitle_tracks(ctx: &ResolveCtx<'_>, embed_url: &str) -> Vec<NuvioSubtitle> {
    let Ok(url) = Url::parse(embed_url) else {
        return Vec::new();
    };
    let Some(sub_info) = url
        .query_pairs()
        .find(|(key, _)| key == "sub.info")
        .map(|(_, value)| value.to_string())
    else {
        return Vec::new();
    };
    let Ok(sub_url) = Url::parse(&sub_info) else {
        return Vec::new();
    };

    let mut body = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
        let request = FetchRequest::get(sub_url.clone())
            .with_header("User-Agent", UA)
            .with_header("Referer", "https://mfw09.org/")
            .with_timeout(Duration::from_secs(10));
        if let Ok(response) = ctx.fetcher.request(request).await
            && response.status == 200
            && !response.body.is_empty()
        {
            body = response.body;
            break;
        }
    }
    let Ok(tracks) = serde_json::from_str::<Value>(&body) else {
        return Vec::new();
    };
    let Some(tracks) = tracks.as_array() else {
        return Vec::new();
    };
    tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| {
            let file = str_field(track, "file")?;
            if !file.starts_with("http://") && !file.starts_with("https://") {
                return None;
            }
            let label = str_field(track, "label").unwrap_or("Subtitles").to_string();
            Some(NuvioSubtitle {
                id: Some(format!("zxc{index}").chars().take(8).collect()),
                url: Some(file.to_string()),
                lang: Some(label.chars().take(8).collect()),
                name: Some(label),
                ..NuvioSubtitle::default()
            })
        })
        .collect()
}

// -- the cards --------------------------------------------------------------

/// One decrypted source → the raw card — the `ZXCStream - {quality}`
/// name, the title line, the mfw09 hotlink headers, and the shared
/// subtitle tracks. `None` when the URL is not playable (the honest
/// drop — Task 55's no-HTML rule).
fn card(source: &Value, title: &str, subtitles: &[NuvioSubtitle]) -> Option<NuvioStream> {
    let url = str_field(source, "url")?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let mime = str_field(source, "mime_type").unwrap_or_default();
    let is_hls =
        mime.to_ascii_lowercase().contains("mpegurl") || url.to_ascii_lowercase().contains(".m3u8");
    let height = source.get("height").and_then(Value::as_u64).unwrap_or(0);
    let quality = str_field(source, "label")
        .map(str::to_string)
        .filter(|label| !label.is_empty())
        .or_else(|| (height > 0).then(|| format!("{height}p")))
        .unwrap_or_else(|| "HD".to_string());

    let mut stream = NuvioStream::new(url)
        .with_name(format!("ZXCStream - {quality}"))
        .with_title(title.to_string())
        .with_quality(quality)
        .with_kind(if is_hls { "hls" } else { "mp4" })
        .with_header("User-Agent", UA)
        .with_header("Referer", "https://mfw09.org/");
    for subtitle in subtitles {
        stream = stream.with_subtitle(subtitle.clone());
    }
    Some(stream)
}

// -- shared plumbing ---------------------------------------------------------

/// The mfw09 API header set — upstream `CHROME_HEADERS` plus
/// `fetchRaw`'s default `Accept`.
fn chrome_headers() -> [(&'static str, &'static str); 7] {
    [
        ("Content-Type", "application/json"),
        ("X-Embed-Origin", "https://player.zxcprime.xyz"),
        (
            "X-Embed-Referer",
            "https://player.zxcprime.xyz/embed/movie/1",
        ),
        ("Origin", "https://mfw09.org"),
        ("Referer", "https://player.zxcprime.xyz/"),
        ("User-Agent", UA),
        ("Accept", "*/*"),
    ]
}

/// The fixed browser fingerprint — upstream `browserFingerprint`
/// verbatim (the hash suffixes are SHA-256 over the fixed labels).
fn browser_fingerprint() -> Value {
    json!({
        "user_agent": UA,
        "pixel_ratio": 1,
        "screen_width": 1920,
        "screen_height": 1080,
        "color_depth": 24,
        "languages": ["en-US", "en"],
        "timezone": "Europe/London",
        "hardware_concurrency": 8,
        "device_memory": 8,
        "touch_points": 0,
        "webgl_vendor": "Google Inc. (Intel)",
        "webgl_renderer": "ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0, D3D11)",
        "canvas_hash": b64url(&Sha256::digest(b"byse-canvas")),
        "audio_hash": b64url(&Sha256::digest(b"byse-audio")),
        "webgl_params_hash": b64url(&Sha256::digest(b"byse-webgl")),
        "fonts_hash": b64url(&Sha256::digest(b"byse-fonts")),
        "codecs_hash": b64url(&Sha256::digest(b"byse-codecs")),
        "media_devices": "ai2ao3vi1",
        "pointer_type": "fine,hover",
        "extra": {
            "vendor": "Google Inc.",
            "appVersion": UA.strip_prefix("Mozilla/").unwrap_or(UA),
        },
    })
}

/// POST a JSON body and parse the JSON response — `fetchRaw` +
/// `JSON.parse` with the strict `=== 200` gate the upstream uses.
async fn post_json(
    ctx: &ResolveCtx<'_>,
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
    timeout: Duration,
) -> Option<Value> {
    let url = Url::parse(url).ok()?;
    let mut request = FetchRequest::post(url, body);
    for (name, value) in headers {
        request = request.with_header(*name, *value);
    }
    let request = request.with_timeout(timeout);
    let response = ctx.fetcher.request(request).await.ok()?;
    if response.status != 200 {
        return None;
    }
    serde_json::from_str(&response.body).ok()
}

/// A string field of a JSON object.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Base64url without padding.
fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Lowercase hex.
fn hex_string(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Base64url decode — the JS's `-`/`_`-tolerant re-pad.
fn b64u_decode(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).ok()
}

/// Wall-clock milliseconds.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0))
}

/// `movie` / `tv` — the media kind the scraper keys on.
fn kind_of(media: &MediaRef) -> &'static str {
    if media.season.is_some() {
        "tv"
    } else {
        "movie"
    }
}

/// The card title — `name [S01E01] (year)` (the scraper's
/// `titleLine`).
fn title_line(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    let episode = if media.season.is_some() {
        format!(" {}", media.format_season_and_episode())
    } else {
        String::new()
    };
    match year {
        Some(year) => format!("{name}{episode} ({year})"),
        None => format!("{name}{episode} ()"),
    }
}

/// The base display title for
/// `build_stream_results` —
/// upstream `name + (season ? ' S01E01' : ' (year)')`.
fn base_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
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

/// The best-effort `IMDb` id — the sentinel query's optional field.
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

mod p256 {
    //! The minimal P-256 ECDSA layer for the Byse attestation: a
    //! compact implementation (affine coordinates over `num-bigint`,
    //! ECDSA with SHA-256 and RFC 6979 deterministic nonces) — the
    //! workspace ships no ECDSA crate and the attestation handshake
    //! needs a client-held key.
    //!
    //! The test vectors pin it against OpenSSL: the public key of a
    //! fixed scalar, and the RFC 6979 A.2.5 signature vectors.

    use std::sync::LazyLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use hmac::{Hmac, Mac};
    use num_bigint::{BigInt, BigUint};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    /// The field prime `p`.
    static P: LazyLock<BigUint> = LazyLock::new(|| {
        BigUint::parse_bytes(
            b"ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
            16,
        )
        .unwrap_or_else(|| panic!("the field prime parses"))
    });
    /// The group order `n`.
    static N: LazyLock<BigUint> = LazyLock::new(|| {
        BigUint::parse_bytes(
            b"ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
            16,
        )
        .unwrap_or_else(|| panic!("the group order parses"))
    });
    /// The generator's x coordinate.
    static GX: LazyLock<BigUint> = LazyLock::new(|| {
        BigUint::parse_bytes(
            b"6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
            16,
        )
        .unwrap_or_else(|| panic!("the generator x parses"))
    });
    /// The generator's y coordinate.
    static GY: LazyLock<BigUint> = LazyLock::new(|| {
        BigUint::parse_bytes(
            b"4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
            16,
        )
        .unwrap_or_else(|| panic!("the generator y parses"))
    });

    /// An affine point; `None` is the point at infinity.
    type Point = Option<(BigUint, BigUint)>;

    /// A P-256 private key — the attestation identity scalar.
    pub(super) struct PrivateKey {
        /// The scalar `d`, in `[1, n)`.
        d: BigUint,
    }

    /// The matching public key point.
    pub(super) struct PublicKey {
        /// The x coordinate.
        x: BigUint,
        /// The y coordinate.
        y: BigUint,
    }

    impl PrivateKey {
        /// A fresh identity — the entropy pool below.
        pub(super) fn generate() -> Self {
            let n = &*N;
            let d = (entropy_scalar() % (n - BigUint::from(1u8))) + BigUint::from(1u8);
            Self { d }
        }

        /// A key from a fixed scalar (the test path).
        #[cfg(test)]
        pub(super) fn from_scalar(d: BigUint) -> Self {
            Self { d }
        }

        /// The scalar, for tests.
        #[cfg(test)]
        pub(super) fn scalar(&self) -> &BigUint {
            &self.d
        }

        /// The public key — `d·G`.
        pub(super) fn public_key(&self) -> PublicKey {
            let Some((x, y)) = scalar_mul(&self.d, Some(generator())) else {
                // A scalar in [1, n) never maps G to infinity.
                panic!("a valid scalar never maps to infinity");
            };
            PublicKey { x, y }
        }

        /// Sign a message — the raw IEEE-P1363 `r ‖ s` layout (the
        /// `WebCrypto` signature format the site's bundle consumes),
        /// with RFC 6979 deterministic nonces.
        pub(super) fn sign(&self, message: &[u8]) -> Vec<u8> {
            let digest = Sha256::digest(message);
            let order = &*N;
            let mut nonces = NonceGen::new(&self.d, &digest);
            let digest_int = BigUint::from_bytes_be(&digest);
            loop {
                let nonce = nonces.next();
                let Some((x, _)) = scalar_mul(&nonce, Some(generator())) else {
                    nonces.reject();
                    continue;
                };
                let r = &x % order;
                let s = if let Some(inverse) = mod_inverse_unsigned(&nonce, order) {
                    (inverse * (&digest_int + &r * &self.d)) % order
                } else {
                    nonces.reject();
                    continue;
                };
                if r != BigUint::default() && s != BigUint::default() {
                    let mut raw = pad32(&r.to_bytes_be());
                    raw.extend_from_slice(&pad32(&s.to_bytes_be()));
                    return raw;
                }
                nonces.reject();
            }
        }
    }

    impl PublicKey {
        /// The WebCrypto-style JWK — `x`/`y` are the base64url of the
        /// 32-byte big-endian coordinates.
        pub(super) fn jwk(&self) -> Value {
            let url = |coordinate: &BigUint| -> String {
                use base64::Engine as _;
                use base64::engine::general_purpose::URL_SAFE_NO_PAD;
                URL_SAFE_NO_PAD.encode(pad32(&coordinate.to_bytes_be()))
            };
            json!({
                "kty": "EC",
                "crv": "P-256",
                "x": url(&self.x),
                "y": url(&self.y),
                "ext": true,
                "key_ops": ["verify"],
            })
        }
    }

    /// The generator point.
    fn generator() -> (BigUint, BigUint) {
        (GX.clone(), GY.clone())
    }

    /// Point addition (the curve's `a = -3`; affine, so each add
    /// carries a modular inverse). The intermediates stay signed —
    /// the JS reference kept negative representatives and normalized
    /// at the end.
    fn add(left: &Point, right: &Point) -> Point {
        let Some((x1, y1)) = left else {
            return right.clone();
        };
        let Some((x2, y2)) = right else {
            return left.clone();
        };
        let p = BigInt::from((*P).clone());
        let x1 = BigInt::from(x1.clone());
        let y1 = BigInt::from(y1.clone());
        let x2 = BigInt::from(x2.clone());
        let y2 = BigInt::from(y2.clone());
        if x1 == x2 {
            if (&y1 + &y2) % &p == BigInt::default() {
                return None;
            }
            // λ = (3x² − 3) / 2y
            let numerator = BigInt::from(3u8) * &x1 * &x1 - BigInt::from(3u8);
            let denominator = BigInt::from(2u8) * &y1;
            let lambda = numerator * mod_inverse(&denominator, &p)? % &p;
            Some(finish(&lambda, &x1, &x2, &y1, &p))
        } else {
            // λ = (y2 − y1) / (x2 − x1)
            let numerator = &y2 - &y1;
            let denominator = &x2 - &x1;
            let lambda = numerator * mod_inverse(&denominator, &p)? % &p;
            Some(finish(&lambda, &x1, &x2, &y1, &p))
        }
    }

    /// The shared tail: `x3 = λ² − x1 − x2`, `y3 = λ(x1 − x3) − y1`.
    fn finish(
        lambda: &BigInt,
        x1: &BigInt,
        x2: &BigInt,
        y1: &BigInt,
        modulus: &BigInt,
    ) -> (BigUint, BigUint) {
        let x3 = lambda * lambda - x1 - x2;
        let y3 = lambda * (x1 - &x3) - y1;
        (reduce(&x3, modulus), reduce(&y3, modulus))
    }

    /// Reduce a possibly-negative value into `[0, modulus)`.
    fn reduce(value: &BigInt, modulus: &BigInt) -> BigUint {
        let reduced = ((value % modulus) + modulus) % modulus;
        reduced
            .to_biguint()
            .unwrap_or_else(|| panic!("a reduced value is non-negative"))
    }

    /// Double-and-add scalar multiplication (little-endian bits).
    fn scalar_mul(scalar: &BigUint, base: Point) -> Point {
        let bits = scalar.to_radix_le(2);
        let mut acc: Point = None;
        let mut current = base;
        for &bit in &bits {
            if bit == 1 {
                acc = add(&acc, &current);
            }
            current = add(&current, &current);
        }
        acc
    }

    /// The modular inverse of a reduced value (extended Euclid over
    /// the signed wrapper).
    fn mod_inverse(value: &BigInt, modulus: &BigInt) -> Option<BigInt> {
        let reduced = ((value % modulus) + modulus) % modulus;
        let (g, x, _) = egcd(&reduced, modulus);
        if g != BigInt::from(1u8) {
            return None;
        }
        Some(((&x % modulus) + modulus) % modulus)
    }

    /// The modular inverse in unsigned form.
    fn mod_inverse_unsigned(value: &BigUint, modulus: &BigUint) -> Option<BigUint> {
        let inverse = mod_inverse(&BigInt::from(value.clone()), &BigInt::from(modulus.clone()))?;
        Some(
            inverse
                .to_biguint()
                .unwrap_or_else(|| panic!("a reduced inverse is non-negative")),
        )
    }

    /// The extended Euclidean algorithm — `(g, x, y)` with
    /// `value·x + modulus·y = g`.
    fn egcd(value: &BigInt, modulus: &BigInt) -> (BigInt, BigInt, BigInt) {
        if *value == BigInt::default() {
            (modulus.clone(), BigInt::default(), BigInt::from(1u8))
        } else {
            let (g, x, y) = egcd(&(modulus % value), value);
            (g, y - (modulus / value) * &x, x)
        }
    }

    /// RFC 6979's deterministic nonce generator over HMAC-SHA-256.
    struct NonceGen {
        /// The running K.
        k_key: Vec<u8>,
        /// The running V.
        v: Vec<u8>,
    }

    impl NonceGen {
        /// The per-message setup (RFC 6979 §3.1 steps a–d).
        fn new(d: &BigUint, digest: &[u8]) -> Self {
            let x = pad32(&d.to_bytes_be());
            let h = pad32(&(&BigUint::from_bytes_be(digest) % &*N).to_bytes_be());
            let mut v = vec![1u8; 32];
            let mut k_key = vec![0u8; 32];
            k_key = hmac(&k_key, &update(&v, 0, &x, &h));
            v = hmac(&k_key, &v);
            k_key = hmac(&k_key, &update(&v, 1, &x, &h));
            v = hmac(&k_key, &v);
            Self { k_key, v }
        }

        /// The candidate loop (§3.2 steps h.1–h.2, with h.3 on
        /// out-of-range candidates).
        fn next(&mut self) -> BigUint {
            loop {
                self.v = hmac(&self.k_key, &self.v);
                let k = BigUint::from_bytes_be(&self.v);
                if k != BigUint::default() && k < *N {
                    return k;
                }
                self.reject();
            }
        }

        /// The rejection update (§3.2 step h.3).
        fn reject(&mut self) {
            let mut input = self.v.clone();
            input.push(0);
            self.k_key = hmac(&self.k_key, &input);
            self.v = hmac(&self.k_key, &self.v);
        }
    }

    /// The HMAC input block: `V ‖ 0x{marker} ‖ int2octets(x) ‖
    /// bits2octets(h1)`.
    fn update(v: &[u8], marker: u8, x: &[u8], h: &[u8]) -> Vec<u8> {
        let mut input = v.to_vec();
        input.push(marker);
        input.extend_from_slice(x);
        input.extend_from_slice(h);
        input
    }

    /// HMAC-SHA-256.
    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key)
            .unwrap_or_else(|_| panic!("HMAC-SHA-256 accepts any key length"));
        mac.update(data);
        mac.finalize().into_bytes().as_slice().to_vec()
    }

    /// Left-pad to 32 bytes.
    fn pad32(bytes: &[u8]) -> Vec<u8> {
        let mut padded = bytes.to_vec();
        while padded.len() < 32 {
            padded.insert(0, 0);
        }
        padded
    }

    /// The keygen entropy pool: OS-seeded hasher states, the clock,
    /// the thread identity, and a stack address (ASLR), distilled
    /// through SHA-256. Adequate for the throwaway per-session
    /// identity this protocol needs (upstream uses the platform
    /// CSPRNG; the workspace has none available).
    fn entropy_scalar() -> BigUint {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let mut pool: Vec<u8> = Vec::new();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0u128, |since| since.as_nanos());
        pool.extend_from_slice(&nanos.to_le_bytes());
        for _ in 0..4 {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write(&pool);
            pool.extend_from_slice(&hasher.finish().to_le_bytes());
        }
        pool.extend_from_slice(format!("{:?}", std::thread::current().id()).as_bytes());
        let probe = 0u8;
        pool.extend_from_slice(&(std::ptr::from_ref(&probe) as usize).to_le_bytes());
        BigUint::from_bytes_be(&Sha256::digest(&pool))
    }
}

/// AES-256-GCM decryption over the raw `aes` block cipher.
///
/// The upstream decrypts the playback config with `node:crypto`'s
/// `aes-256-gcm`; the workspace ships no GCM crate, so this is the
/// same minimal SP 800-38D decrypt-and-verify (empty associated
/// data) the vidzee host uses, with the tag as the payload's last 16
/// bytes (the `WebCrypto` layout).
mod gcm {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::{Aes256, Block};

    /// The GHASH reduction constant `R = E1 ‖ 0^120`.
    const R: u128 = 0xE1 << 120;

    /// Decrypt `ciphertext` under `key`, verifying `auth_tag`.
    ///
    /// Returns `None` on length errors or tag mismatch — the
    /// upstream `decipher.final()` throw.
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

        // J0 = IV ‖ 0^31 ‖ 1 (the 96-bit-IV case).
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;

        // T = E_K(J0) ⊕ GHASH_H(C) over the (empty) AAD.
        let h = e_k(&cipher, &[0u8; 16]);
        let expected = xor(&e_k(&cipher, &j0), &ghash(h, ciphertext));
        if !constant_time_eq(&expected, auth_tag) {
            return None;
        }

        // CTR keystream from inc32(J0); the first data block uses J0+1.
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

    /// GHASH with empty associated data: `y_i = (y_{i-1} ⊕ X_i) • H`
    /// over the ciphertext blocks (zero-padded) and the bit-length
    /// block.
    fn ghash(h: [u8; 16], data: &[u8]) -> [u8; 16] {
        let h = u128::from_be_bytes(h);
        let mut y: u128 = 0;
        let mut block = [0u8; 16];
        for chunk in data.chunks(16) {
            block.fill(0);
            block[..chunk.len()].copy_from_slice(chunk);
            y = gmul(y ^ u128::from_be_bytes(block), h);
        }
        // [len(AAD) = 0 (bits) ‖ len(data) (bits)], big-endian.
        block.fill(0);
        block[8..].copy_from_slice(&u64::try_from(data.len() * 8).unwrap_or(0).to_be_bytes());
        y = gmul(y ^ u128::from_be_bytes(block), h);
        y.to_be_bytes()
    }

    /// Multiplication in GF(2^128) with the GCM reduction polynomial.
    fn gmul(left: u128, right: u128) -> u128 {
        let mut product: u128 = 0;
        let mut value = left;
        for bit in (0..128).rev() {
            if (right >> bit) & 1 == 1 {
                product ^= value;
            }
            let lsb = value & 1;
            value >>= 1;
            if lsb == 1 {
                value ^= R;
            }
        }
        product
    }

    /// `inc32`: increment the big-endian last 32 bits of the block.
    fn increment_counter(counter: &mut [u8; 16]) {
        let value = u32::from_be_bytes([counter[12], counter[13], counter[14], counter[15]]);
        let incremented = value.wrapping_add(1);
        counter[12..].copy_from_slice(&incremented.to_be_bytes());
    }

    /// XOR two blocks.
    fn xor(left: &[u8; 16], right: &[u8; 16]) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (byte, (l, r)) in out.iter_mut().zip(left.iter().zip(right)) {
            *byte = l ^ r;
        }
        out
    }

    /// Constant-time comparison of two equally long slices.
    fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
        if left.len() != right.len() {
            return false;
        }
        let mut diff = 0u8;
        for (l, r) in left.iter().zip(right) {
            diff |= l ^ r;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex, PoisonError};

    use num_bigint::BigUint;

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

        /// The (first) recorded request whose path contains `needle`.
        fn request_to(&self, needle: &str) -> Option<FetchRequest> {
            self.requests()
                .into_iter()
                .find(|request| request.url.as_str().contains(needle))
        }

        /// The JSON body of the request whose path contains `needle`.
        fn body_of(&self, needle: &str) -> Option<Value> {
            self.request_to(needle)
                .and_then(|request| request.body.clone())
                .and_then(|body| serde_json::from_str(&body).ok())
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

    // -- fixtures ------------------------------------------------------------

    /// The fixture media id (Dune: Part Two).
    const TMDB: u64 = 693_134;

    /// A provider over the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> ZXCStream {
        ZXCStream::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
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

    /// The fixture token response — obfuscated field names.
    fn token_page() -> String {
        format!(
            r#"{{"{}":"tok-1","{}":"1750000000000"}}"#,
            field_map::TOKEN,
            field_map::TS
        )
    }

    /// The fixture sentinel response.
    fn sentinel_page() -> String {
        r#"{"embed":"https://mfw09.org/e/vid-9?sub.info=https://qqgcdn.cloud/s/1/subs.json"}"#
            .to_string()
    }

    /// The fixture challenge response.
    fn challenge_page() -> String {
        r#"{"challenge_id":"chal-1","nonce":"challenge-nonce-42"}"#.to_string()
    }

    /// The fixture attestation response.
    fn attest_page() -> String {
        r#"{"viewer_id":"viewer-1","device_id":"device-1","confidence":0.87}"#.to_string()
    }

    /// The fixture captcha response — difficulty 10 so the `PoW`
    /// (solution 241, precomputed with the JS) stays fast.
    fn captcha_page() -> String {
        r#"{"pow_nonce":"nonce","pow_difficulty":10,"pow_token":"ptok"}"#.to_string()
    }

    /// The fixture verify response.
    fn verify_page() -> String {
        r#"{"status":"ok","token":"ctok","expires_in":600}"#.to_string()
    }

    /// The 30-part key table — part `i` is 16 bytes of `i + 1`.
    fn key_parts() -> Vec<String> {
        (1..=30).map(|byte: u8| b64url(&[byte; 16])).collect()
    }

    /// The fixture playback response — version 15, so the key is
    /// `parts[14] ‖ parts[15]` (`0f…0f 10…10`), with a node-generated
    /// GCM payload over the two-source plaintext.
    fn playback_page() -> String {
        let payload = "78cwsFCnLN6Hqx1jUZd66QZCq-6qlCFooFAz8WRfNjJL5h536xro2JWpgN4E11zGVc1l5CEFmdsjF3UrJucgYfQIHVjNqSuQ7f1fL-xy0rwHOFR0M1EWGygv34NUnfa1p3ygy8DAuS5v6KN_jdpTcYdvNs7gUEs9XQpY1yT8G4NziXPPAUy4krxdPXYqnauBo4rFOOIJ8zqsLFpJFRGmRZwShz2FBh3M72P_CCO9WAXCF1uJuiUDpsc8HAFqfOwOajD9K3K8UH13HwF1Ru5z3_QcYs0OvWx-eca92z3J9tPPAAdcZd-AessknTVz7v3UdCKZ1X586ucpEuaZiDzLTonm6q8iySr5ct34Aw4x-3xijbP3euRF-sxFPWrM2XCOI7E3ODAJNbu-kzYALUOP6ppAYIz93vbSOwD2ZolelRec04twhEkI7-QhTCh5WLR9k4dwepm2xcyZ1k128GsnYJSGynz050T4ZqmpCEMkGWMWFQKOh1lRjNwSBb7PmzF_XTOssAjDUSwY5jLTHob1wE_QAUyDKW1qYNWaKEB8ObM";
        // Assemble {version, key_parts, iv, payload} with version 15.
        let parts = key_parts();
        json!({
            "playback": {
                "version": 15,
                "key_parts": parts,
                "iv": b64url(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb]),
                "payload": payload,
            }
        })
        .to_string()
    }

    /// The fixture sub.info list.
    fn subs_page() -> String {
        r#"[{"file":"https://qqgcdn.cloud/s/1/en.vtt","label":"English"},{"file":"not-a-url","label":"Broken"}]"#
            .to_string()
    }

    /// The full happy-path mock — every endpoint of the protocol.
    fn happy_path() -> MockFetcher {
        MockFetcher::new()
            .serve("player.zxcprime.xyz/backend/ololmo", 200, token_page())
            .serve(
                "player.zxcprime.xyz/backend_/embed/sentinel",
                200,
                sentinel_page(),
            )
            .serve(
                "mfw09.org/api/videos/access/challenge",
                200,
                challenge_page(),
            )
            .serve("mfw09.org/api/videos/access/attest", 200, attest_page())
            .serve(
                "mfw09.org/api/videos/vid-9/embed/captcha",
                200,
                captcha_page(),
            )
            .serve(
                "mfw09.org/api/videos/vid-9/embed/captcha/verify",
                200,
                verify_page(),
            )
            .serve(
                "mfw09.org/api/videos/vid-9/embed/playback",
                200,
                playback_page(),
            )
            .serve("qqgcdn.cloud/s/1/subs.json", 200, subs_page())
    }

    // -- the crypto vectors (node/OpenSSL ground truth) ----------------------

    /// `generateFrontendToken` — the hex `fToken` prefix (verified
    /// with node's `crypto`).
    #[test]
    fn frontend_token_matches_the_js_hash() {
        // The ts is current time, so pin the hash instead: the same
        // input must produce the same 64-hex prefix (verified with
        // node's crypto).
        let digest = Sha512::digest(format!("1000:{SECRET}:{TMDB}").as_bytes());
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        assert_eq!(
            &hex[..64],
            "65880cd65ad0d9c53b487446773c267f2a243281eecd4f092fcf1b3e92a67556"
        );
        // The live token always renders as 64 lowercase hex chars.
        let token = generate_frontend_token(TMDB);
        assert_eq!(token.xt.len(), 64);
        assert!(
            token
                .xt
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        );
    }

    /// The `PoW` hash — three node-computed digests.
    #[test]
    fn byse_hash_matches_the_js() {
        let hex = |words: &[u32; 8]| -> String {
            use std::fmt::Write as _;
            let mut out = String::with_capacity(32);
            for word in words {
                let _ = write!(out, "{word:08x}");
            }
            out
        };
        assert_eq!(
            hex(&byse_hash(b"")),
            "8703893e9260f0cf2bf5d1d7a35805bc9cd563e323d5283b42cd9b249f2fef93"
        );
        assert_eq!(
            hex(&byse_hash(b"a")),
            "943cfd8c92c17927a34d9eb5602fdae4517a8003380b53018337d0491a31f1ed"
        );
        assert_eq!(
            hex(&byse_hash(b"nonce:7")),
            "d48f281e2df64d59114e25f4bf59b95d6b20e7dacf576d21669b2c244c6692b6"
        );
    }

    /// The `PoW` solver — node-computed solutions.
    #[test]
    fn pow_solutions_match_the_js() {
        assert_eq!(
            solve_byse_pow("nonce", 10, Duration::from_secs(30)).as_deref(),
            Some("241")
        );
        assert_eq!(
            solve_byse_pow("abc", 9, Duration::from_secs(30)).as_deref(),
            Some("131")
        );
        assert_eq!(
            solve_byse_pow("anything", 0, Duration::from_secs(1)).as_deref(),
            Some("0")
        );
    }

    /// P-256: the public key of the fixed RFC 6979 test scalar —
    /// byte-for-byte the OpenSSL output.
    #[test]
    fn public_key_matches_openssl() {
        let d = BigUint::parse_bytes(
            b"C9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721",
            16,
        )
        .unwrap_or_else(|| panic!("the test scalar parses"));
        let key = p256::PrivateKey::from_scalar(d);
        let jwk = key.public_key().jwk();
        let mut raw = vec![0x04];
        for coordinate in ["x", "y"] {
            let bytes = super::b64u_decode(
                jwk.get(coordinate)
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
            .unwrap_or_else(|| panic!("the {coordinate} coordinate decodes"));
            raw.extend_from_slice(&bytes);
        }
        assert_eq!(
            hex(&raw),
            "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb67903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299"
        );
    }

    /// P-256: the RFC 6979 A.2.5 "sample" vector — the published
    /// deterministic signature.
    #[test]
    fn sign_matches_rfc6979_sample_vector() {
        let d = BigUint::parse_bytes(
            b"C9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721",
            16,
        )
        .unwrap_or_else(|| panic!("the test scalar parses"));
        let key = p256::PrivateKey::from_scalar(d);
        let raw = key.sign(b"sample");
        assert_eq!(raw.len(), 64);
        assert_eq!(
            hex(&raw[..32]),
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716"
        );
        assert_eq!(
            hex(&raw[32..]),
            "f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8"
        );
    }

    /// P-256: the node/OpenSSL-cross-checked signature over the
    /// fixture challenge nonce (and determinism).
    #[test]
    fn sign_matches_the_node_cross_check() {
        let d = BigUint::parse_bytes(
            b"C9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721",
            16,
        )
        .unwrap_or_else(|| panic!("the test scalar parses"));
        let key = p256::PrivateKey::from_scalar(d);
        let first = key.sign(b"challenge-nonce-42");
        let second = key.sign(b"challenge-nonce-42");
        // Deterministic nonces: byte-identical signatures.
        assert_eq!(first, second);
        assert_eq!(
            hex(&first[..32]),
            "75d3ca363ebba746611794acf4fb2051f3a97a93fe60078d458507e40ba596ff"
        );
        assert_eq!(
            hex(&first[32..]),
            "9a2b76ed88f3396c54aa030370dcf7c218e7a20fe4b212455f465430f5dd45a1"
        );
    }

    /// P-256: fresh identities differ.
    #[test]
    fn generated_keys_differ() {
        let first = p256::PrivateKey::generate();
        let second = p256::PrivateKey::generate();
        assert_ne!(first.scalar(), second.scalar());
    }

    /// AES-256-GCM: the version-15 key-parts vector.
    #[test]
    fn gcm_decrypts_the_playback_payload() {
        let key: Vec<u8> = [0x0fu8; 16]
            .iter()
            .chain([0x10u8; 16].iter())
            .copied()
            .collect();
        let iv = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
        ];
        let payload = b64u_decode(
            "78cwsFCnLN6Hqx1jUZd66QZCq-6qlCFooFAz8WRfNjJL5h536xro2JWpgN4E11zGVc1l5CEFmdsjF3UrJucgYfQIHVjNqSuQ7f1fL-xy0rwHOFR0M1EWGygv34NUnfa1p3ygy8DAuS5v6KN_jdpTcYdvNs7gUEs9XQpY1yT8G4NziXPPAUy4krxdPXYqnauBo4rFOOIJ8zqsLFpJFRGmRZwShz2FBh3M72P_CCO9WAXCF1uJuiUDpsc8HAFqfOwOajD9K3K8UH13HwF1Ru5z3_QcYs0OvWx-eca92z3J9tPPAAdcZd-AessknTVz7v3UdCKZ1X586ucpEuaZiDzLTonm6q8iySr5ct34Aw4x-3xijbP3euRF-sxFPWrM2XCOI7E3ODAJNbu-kzYALUOP6ppAYIz93vbSOwD2ZolelRec04twhEkI7-QhTCh5WLR9k4dwepm2xcyZ1k128GsnYJSGynz050T4ZqmpCEMkGWMWFQKOh1lRjNwSBb7PmzF_XTOssAjDUSwY5jLTHob1wE_QAUyDKW1qYNWaKEB8ObM",
        )
        .unwrap_or_else(|| panic!("the fixture payload decodes"));
        let (ciphertext, tag) = payload.split_at(payload.len() - 16);
        let plain = gcm::decrypt(&key, &iv, tag, ciphertext)
            .unwrap_or_else(|| panic!("the fixture payload decrypts"));
        let config: Value =
            serde_json::from_slice(&plain).unwrap_or_else(|e| panic!("the plaintext is JSON: {e}"));
        let sources = config
            .get("sources")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("the config has sources"));
        assert_eq!(sources.len(), 2);
        assert_eq!(
            str_field(&sources[0], "url"),
            Some("https://sprintcdn.example.org/hls/dune/master.m3u8?tok=abc")
        );
        // A tampered tag fails.
        let mut bad = payload.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        let (ciphertext, tag) = bad.split_at(bad.len() - 16);
        assert!(gcm::decrypt(&key, &iv, tag, ciphertext).is_none());
    }

    /// The key-parts assembly — the 1-indexed pair and the fallback.
    #[test]
    fn key_parts_assemble_the_pairs() {
        let parts = key_parts();
        let v15 = json!({ "version": 15, "key_parts": parts });
        let key = assemble_key(&v15).unwrap_or_else(|| panic!("v15 assembles"));
        assert_eq!(
            key,
            [0x0fu8; 16]
                .iter()
                .chain([0x10u8; 16].iter())
                .copied()
                .collect::<Vec<u8>>()
        );

        let parts = key_parts();
        let v1 = json!({ "version": 1, "key_parts": parts });
        let key = assemble_key(&v1).unwrap_or_else(|| panic!("v1 assembles"));
        assert_eq!(
            key,
            [0x01u8; 16]
                .iter()
                .chain([0x1eu8; 16].iter())
                .copied()
                .collect::<Vec<u8>>()
        );
    }

    /// The browser fingerprint — the fixed hash suffixes.
    #[test]
    fn fingerprint_hashes_are_the_js_values() {
        let fingerprint = browser_fingerprint();
        assert_eq!(
            fingerprint.get("canvas_hash").and_then(Value::as_str),
            Some("abpHlJSwXPMXNUYniyWtsxh7mcIsqYLIa0ZANa40K-U")
        );
        assert_eq!(
            fingerprint.get("audio_hash").and_then(Value::as_str),
            Some("OjXCEPZLzWvPmI88W-CvTRM6ywZI4jjGH5echR2J3Kg")
        );
        assert_eq!(
            fingerprint.get("webgl_params_hash").and_then(Value::as_str),
            Some("P3_wh58dDhYOZmk3wz7hejSHZ_uK1mMAaU6RBY0ofXc")
        );
    }

    /// The embed code — the last path segment.
    #[test]
    fn embed_code_is_the_last_segment() {
        assert_eq!(
            embed_code("https://mfw09.org/e/vid-9?sub.info=https://x/y").as_deref(),
            Some("vid-9")
        );
        assert_eq!(embed_code("https://mfw09.org/").as_deref(), None);
    }

    // -- the protocol flow ---------------------------------------------------

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        assert_eq!(info.id, "zxcstream");
        assert_eq!(info.label, "ZXCStream");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://player.zxcstream.xyz/")
        );
        assert_eq!(info.priority, 0);
    }

    /// The whole protocol is one linear walkthrough (captcha → seal →
    /// key parts → playback decrypt); splitting it would hide the
    /// ordering the port preserves.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolves_through_the_whole_protocol() -> Result<(), SourceError> {
        let fetcher = Arc::new(happy_path());
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await
            .unwrap_or_else(|e| panic!("the happy path must resolve: {e}"));

        // The decrypted config carried two sources (HLS + MP4).
        assert_eq!(streams.len(), 2);
        let first = &streams[0];
        assert_eq!(
            first.url.as_str(),
            "https://sprintcdn.example.org/hls/dune/master.m3u8?tok=abc"
        );
        assert_eq!(first.format, Format::Hls);
        assert_eq!(first.meta.resolution, Some(1080));
        assert_eq!(first.meta.source_id.as_deref(), Some("zxcstream"));
        assert_eq!(first.ttl, TTL);
        // The mfw09 hotlink headers.
        assert_eq!(
            first
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://mfw09.org/")
        );
        assert_eq!(
            first
                .meta
                .request_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(UA)
        );
        // The binge groups.
        assert_eq!(
            first.behavior_hints.get("bingeGroup").map(String::as_str),
            Some("zxcstream-hls")
        );
        assert_eq!(
            streams[1]
                .behavior_hints
                .get("bingeGroup")
                .map(String::as_str),
            Some("zxcstream-mp4")
        );
        // The sub.info tracks rode along (the broken one dropped).
        assert_eq!(first.meta.subtitles.len(), 1);
        assert_eq!(
            first.meta.subtitles[0].url.as_str(),
            "https://qqgcdn.cloud/s/1/en.vtt"
        );
        // The label: base title — card title — filename.
        let label = first.label.as_deref().unwrap_or_default();
        assert!(
            label.starts_with("Dune: Part Two (2024) — Dune: Part Two (2024)"),
            "{label}"
        );

        // Stage A: the token POST body used the obfuscated fields.
        let token_body = fetcher
            .body_of("backend/ololmo")
            .unwrap_or_else(|| panic!("the token POST fired"));
        assert_eq!(
            token_body.get(field_map::ID).and_then(Value::as_str),
            Some("693134")
        );
        assert_eq!(
            token_body.get(field_map::PATH).and_then(Value::as_str),
            Some("https://player.zxcprime.xyz/embed/movie/693134")
        );
        assert_eq!(
            token_body
                .get(field_map::MEDIA_TYPE)
                .and_then(Value::as_str),
            Some("movie")
        );
        assert!(token_body.get(field_map::TS).is_some());
        assert!(token_body.get(field_map::FTOKEN).is_some());
        // The token POST headers.
        let token_request = fetcher
            .request_to("backend/ololmo")
            .unwrap_or_else(|| panic!("the token POST fired"));
        assert!(token_request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Origin") && value == "https://player.zxcprime.xyz"
        }));
        assert!(token_request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Referer")
                && value == "https://player.zxcprime.xyz/embed/movie/693134"
        }));

        // The sentinel query used the obfuscated keys.
        let sentinel = fetcher
            .request_to("embed/sentinel")
            .unwrap_or_else(|| panic!("the sentinel GET fired"));
        let query = sentinel.url.query().unwrap_or_default();
        assert!(
            query.contains(&format!("{}=693134", field_map::ID)),
            "{query}"
        );
        assert!(query.contains("b=movie"), "{query}");
        assert!(
            query.contains(&format!("{}=tok-1", field_map::TOKEN)),
            "{query}"
        );

        // Stage B: the attestation body carried a raw P-256 signature
        // (64 bytes → 86 base64url chars) and the JWK.
        let attest_body = fetcher
            .body_of("access/attest")
            .unwrap_or_else(|| panic!("the attest POST fired"));
        let signature = attest_body
            .get("signature")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(signature.len(), 86);
        assert_eq!(
            attest_body.get("nonce").and_then(Value::as_str),
            Some("challenge-nonce-42")
        );
        let jwk = attest_body.get("public_key").cloned().unwrap_or_default();
        assert_eq!(jwk.get("kty").and_then(Value::as_str), Some("EC"));
        assert_eq!(jwk.get("crv").and_then(Value::as_str), Some("P-256"));
        assert_eq!(jwk.get("x").and_then(Value::as_str).map(str::len), Some(43));
        assert_eq!(jwk.get("y").and_then(Value::as_str).map(str::len), Some(43));
        assert!(attest_body.get("client").is_some());

        // Stage C: the verify body carried the precomputed solution.
        let verify_body = fetcher
            .body_of("captcha/verify")
            .unwrap_or_else(|| panic!("the verify POST fired"));
        assert_eq!(
            verify_body.get("pow_token").and_then(Value::as_str),
            Some("ptok")
        );
        assert_eq!(
            verify_body.get("solution").and_then(Value::as_str),
            Some("241")
        );

        // Stage D: the playback body carried the snake_case
        // fingerprint and the captcha token header.
        let playback_body = fetcher
            .body_of("embed/playback")
            .unwrap_or_else(|| panic!("the playback POST fired"));
        let fingerprint = playback_body
            .get("fingerprint")
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            fingerprint.get("viewer_id").and_then(Value::as_str),
            Some("viewer-1")
        );
        assert_eq!(
            fingerprint.get("device_id").and_then(Value::as_str),
            Some("device-1")
        );
        let playback_request = fetcher
            .request_to("embed/playback")
            .unwrap_or_else(|| panic!("the playback POST fired"));
        assert!(playback_request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("X-Captcha-Token") && value == "ctok"
        }));
        Ok(())
    }

    #[tokio::test]
    async fn the_attestation_is_cached_across_resolves() {
        let fetcher = Arc::new(happy_path());
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let source = provider(&fetcher);
        let media = MediaRef::movie(MediaId::Tmdb(TMDB));

        let first = source.resolve(&ctx, &media).await;
        let second = source.resolve(&ctx, &media).await;
        assert!(first.is_ok());
        assert!(second.is_ok());
        // One challenge + one attest: the identity is reused.
        let challenges = fetcher
            .requests()
            .iter()
            .filter(|request| request.url.path().contains("access/challenge"))
            .count();
        let attests = fetcher
            .requests()
            .iter()
            .filter(|request| request.url.path().contains("access/attest"))
            .count();
        assert_eq!(challenges, 1);
        assert_eq!(attests, 1);
    }

    #[tokio::test]
    async fn the_token_route_rotates_to_the_fallback() {
        // ololmo 404s (the Task 86 rotation); burat answers.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("player.zxcprime.xyz/backend/ololmo", 404, "")
                .serve("player.zxcprime.xyz/backend/burat", 200, token_page())
                .serve(
                    "player.zxcprime.xyz/backend_/embed/sentinel",
                    200,
                    sentinel_page(),
                )
                .serve(
                    "mfw09.org/api/videos/access/challenge",
                    200,
                    challenge_page(),
                )
                .serve("mfw09.org/api/videos/access/attest", 200, attest_page())
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha",
                    200,
                    captcha_page(),
                )
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha/verify",
                    200,
                    verify_page(),
                )
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/playback",
                    200,
                    playback_page(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await
            .unwrap_or_else(|e| panic!("the rotated route must resolve: {e}"));
        assert_eq!(streams.len(), 2);
        assert!(fetcher.request_to("backend/ololmo").is_some());
        assert!(fetcher.request_to("backend/burat").is_some());
    }

    #[tokio::test]
    async fn no_embed_resolved_is_not_found() {
        // Every token route is down on both bases.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("player.zxcprime.xyz/backend/ololmo", 404, "")
                .serve("player.zxcprime.xyz/backend/burat", 500, "")
                .serve("player.zxcprime.xyz/backend/bugok", 404, "")
                .serve("player.zxcprime.xyz/backend/abaygagoka", 403, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
        // Nothing reached the Byse API.
        assert!(fetcher.request_to("mfw09.org").is_none());
    }

    #[tokio::test]
    async fn attestation_failure_is_not_found() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("player.zxcprime.xyz/backend/ololmo", 200, token_page())
                .serve(
                    "player.zxcprime.xyz/backend_/embed/sentinel",
                    200,
                    sentinel_page(),
                )
                .serve("mfw09.org/api/videos/access/challenge", 403, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
        // The captcha never fired.
        assert!(fetcher.request_to("embed/captcha").is_none());
    }

    #[tokio::test]
    async fn the_playback_gate_is_an_honest_zero() {
        // Upstream's live state: the playback POST 403s ("upstream
        // gate — honest zero until it relents").
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("player.zxcprime.xyz/backend/ololmo", 200, token_page())
                .serve(
                    "player.zxcprime.xyz/backend_/embed/sentinel",
                    200,
                    sentinel_page(),
                )
                .serve(
                    "mfw09.org/api/videos/access/challenge",
                    200,
                    challenge_page(),
                )
                .serve("mfw09.org/api/videos/access/attest", 200, attest_page())
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha",
                    200,
                    captcha_page(),
                )
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha/verify",
                    200,
                    verify_page(),
                )
                .serve("mfw09.org/api/videos/vid-9/embed/playback", 403, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
        // The whole ladder fired first.
        assert!(fetcher.request_to("embed/captcha/verify").is_some());
        assert!(fetcher.request_to("embed/playback").is_some());
    }

    #[tokio::test]
    async fn a_bad_decrypt_is_not_found() {
        // The payload decrypts to garbage (tampered tag): honest zero.
        let mut page = playback_page();
        page = page.replace("\"payload\":\"78cwsFCn", "\"payload\":\"88cwsFCn");
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("player.zxcprime.xyz/backend/ololmo", 200, token_page())
                .serve(
                    "player.zxcprime.xyz/backend_/embed/sentinel",
                    200,
                    sentinel_page(),
                )
                .serve(
                    "mfw09.org/api/videos/access/challenge",
                    200,
                    challenge_page(),
                )
                .serve("mfw09.org/api/videos/access/attest", 200, attest_page())
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha",
                    200,
                    captcha_page(),
                )
                .serve(
                    "mfw09.org/api/videos/vid-9/embed/captcha/verify",
                    200,
                    verify_page(),
                )
                .serve("mfw09.org/api/videos/vid-9/embed/playback", 200, page),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn series_carries_the_season_episode_query() {
        let fetcher = Arc::new(happy_path());
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref(),
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: Some("tt0903747".to_string()),
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(2),
                episode: Some(3),
            }),
            source_id: None,
            referer: None,
        };

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 2, 3))
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 2);
        // The token body and sentinel query switched to the series
        // shape.
        let token_body = fetcher.body_of("backend/ololmo").unwrap_or_default();
        assert_eq!(
            token_body
                .get(field_map::MEDIA_TYPE)
                .and_then(Value::as_str),
            Some("tv")
        );
        assert_eq!(
            token_body.get(field_map::PATH).and_then(Value::as_str),
            Some("https://player.zxcprime.xyz/embed/tv/1396")
        );
        let sentinel = fetcher
            .request_to("embed/sentinel")
            .unwrap_or_else(|| panic!("the sentinel GET fired"));
        let query = sentinel.url.query().unwrap_or_default();
        assert!(query.contains("b=tv"), "{query}");
        assert!(
            query.contains(&format!("{}=2", field_map::SEASON)),
            "{query}"
        );
        assert!(
            query.contains(&format!("{}=3", field_map::EPISODE)),
            "{query}"
        );
        // The title line carries S02E03.
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(label.contains("Breaking Bad S02E03 (2008)"), "{label}");
    }

    // -- helpers --------------------------------------------------------------

    /// Lowercase hex.
    fn hex(bytes: &[u8]) -> String {
        super::hex_string(bytes)
    }
}
