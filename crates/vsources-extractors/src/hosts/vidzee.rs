//! `VidZee`: TMDB-keyed HLS from `player.vidzee.wtf` embeds.
//!
//! Current embeds first use `core.vidzee.wtf/streams/<movie|tv>/…` with
//! the English/default servers, preserving API headers and the browser's
//! implicit Referer. The old AES API below remains a compatibility fallback.
//! Ports `src/extractor/VidZee.js`:
//!
//! 1. Parse the tmdb id / season / episode from the embed path
//!    (`…/movie/{id}` or `…/tv/{id}/{s}/{e}`); the server comes from
//!    `?sr=`, defaulting to `4`.
//! 2. Fetch and decrypt the rotating API key: `core.vidzee.wtf/api-key`
//!    returns an AES-256-GCM payload (`iv ‖ tag ‖ ciphertext`, base64)
//!    keyed by `sha256` of the site seed; the decrypted key is cached for
//!    an hour (instance state, ports the upstream module cache).
//! 3. `player.vidzee.wtf/api/server?id=…&sr=…[&ss=…&ep=…]` lists stream
//!    entries whose `link` decrypts (AES-256-CBC, the API key zero-padded
//!    to 32 bytes) to the direct file URL, shipped with its hotlink
//!    `Referer` (and the site-specified `User-Agent` when the API sends
//!    one).
//!
//! Cut from the upstream: the `VIDZEE_AES_SEED` environment override of
//! `site-secrets.cjs` — a library carries no environment configuration,
//! so the shipped default seed is used; and `meta.title` — `StreamMeta`
//! has no title field, the stream label carries the server name and
//! language instead.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream, StreamMeta};

/// Ports `VIDZEE_AES_SEED` — the site-secrets default the API key is
/// derived from.
const ENCRYPTION_KEY_SECRET: &str = "4f2a9c7d1e8b3a6f0d5c2e9a7b1f4d8c";

/// The rotating API-key endpoint.
const API_KEY_URL: &str = "https://core.vidzee.wtf/api-key";

/// The server listing endpoint.
const SERVER_API_URL: &str = "https://player.vidzee.wtf/api/server";

/// How long the decrypted API key stays cached (upstream: 1h).
const API_KEY_TTL: Duration = Duration::from_secs(3600);

/// Upstream result lifetime: 3h.
const TTL: Duration = Duration::from_hours(3);

static HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});

/// The `VidZee` extractor.
#[derive(Debug)]
pub struct VidZee {
    /// Current API results, coalesced across the legacy server URLs.
    current_cache: moka::future::Cache<String, Arc<Vec<Stream>>>,
    /// The decrypted API key with its fetch time (1h TTL).
    api_key_cache: Mutex<Option<(Instant, String)>>,
}

impl Default for VidZee {
    fn default() -> Self {
        Self {
            api_key_cache: Mutex::new(None),
            current_cache: moka::future::Cache::builder()
                .max_capacity(80)
                .time_to_live(Duration::from_mins(5))
                .build(),
        }
    }
}

impl VidZee {
    /// A new extractor; the API key cache starts cold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Extractor for VidZee {
    fn id(&self) -> &'static str {
        "vidzee"
    }

    fn label(&self) -> &'static str {
        "VidZee"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| host == "player.vidzee.wtf" || host.ends_with(".vidzee.wtf"))
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let parsed = ParsedEmbed::from_url(url);
        // `if (!tmdbId) return []`.
        let Some(tmdb_id) = parsed.tmdb_id.as_deref() else {
            return Err(ExtractorError::NotFound);
        };
        let current_key = format!("{}:{:?}:{:?}", tmdb_id, parsed.season, parsed.episode);
        if let Ok(streams) = self
            .current_cache
            .try_get_with(current_key, async {
                let streams = current_streams(ctx, &parsed).await;
                if streams.is_empty() {
                    Err(ExtractorError::NotFound)
                } else {
                    Ok(Arc::new(streams))
                }
            })
            .await
        {
            return Ok(streams.as_ref().clone());
        }
        // A failed key fetch/decrypt is the upstream's caught `null`.
        let Some(api_key) = self.get_api_key(ctx).await else {
            return Err(ExtractorError::NotFound);
        };

        // `apiUrl.searchParams.set(…)`: id + sr, plus ss/ep for series.
        let api_url = if let Some(season) = parsed.season.as_deref() {
            format!(
                "{SERVER_API_URL}?id={tmdb_id}&sr={}&ss={season}&ep={}",
                parsed.server_id,
                parsed.episode.as_deref().unwrap_or("1"),
            )
        } else {
            format!("{SERVER_API_URL}?id={tmdb_id}&sr={}", parsed.server_id)
        };
        let api_url =
            Url::parse(&api_url).unwrap_or_else(|e| panic!("the server API URL must parse: {e}"));
        // Upstream lets `fetcher.json` errors escape `extractInternal`.
        let response = ctx.fetcher.request(FetchRequest::get(api_url)).await?;
        let server: Value = response.json().map_err(ExtractorError::from)?;

        // `serverResponse.error || !serverResponse.url?.length` → [].
        let has_error = str_field(&server, "error").is_some_and(|message| !message.is_empty());
        let entries = match server.get("url").and_then(Value::as_array) {
            Some(list) if !has_error && !list.is_empty() => list,
            _ => return Err(ExtractorError::NotFound),
        };
        let site_user_agent = server_user_agent(&server);

        let mut streams = Vec::new();
        for entry in entries {
            let Some(link) = str_field(entry, "link") else {
                continue;
            };
            // `if (!decryptedUrl) continue`.
            let Some(decrypted) = decrypt_server_url(link, &api_key) else {
                continue;
            };

            // `type === 'hls' || decryptedUrl.includes('.m3u8')`.
            let format = if str_field(entry, "type") == Some("hls") || decrypted.contains(".m3u8") {
                Format::Hls
            } else {
                Format::Mp4
            };
            let stream_url = Url::parse(&decrypted).map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid decrypted stream URL: {e}"))
            })?;

            // The height probe is best-effort and only runs for HLS.
            let resolution = if format == Format::Hls {
                let mut headers = BTreeMap::new();
                if let Some(user_agent) = site_user_agent {
                    headers.insert("User-Agent".to_string(), user_agent.to_string());
                }
                guess_height_from_playlist(ctx, &stream_url, &headers).await
            } else {
                None
            };
            let meta = StreamMeta {
                resolution,
                ..StreamMeta::default()
            };
            // `label: \`${name} (${flag}) - ${lang}\`` and the hotlink
            // headers the player must send.
            let label = format!(
                "{} ({}) - {}",
                str_field(entry, "name").unwrap_or_default(),
                str_field(entry, "flag").unwrap_or_default(),
                str_field(entry, "lang").unwrap_or_default()
            );
            let meta = meta.with_header("Referer", "https://player.vidzee.wtf/");
            let meta = if let Some(user_agent) = site_user_agent {
                meta.with_header("User-Agent", user_agent)
            } else {
                meta
            };
            let mut stream = Stream::new(stream_url, format)
                .with_label(label)
                .with_ttl(TTL);
            stream.meta = meta;
            streams.push(stream);
        }

        if streams.is_empty() {
            return Err(ExtractorError::NotFound);
        }
        Ok(streams)
    }
}

impl VidZee {
    /// The (cached) decrypted API key, ports `getApiKey`.
    async fn get_api_key(&self, ctx: &ResolveCtx<'_>) -> Option<String> {
        {
            let cached = self
                .api_key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((fetched_at, key)) = cached.as_ref()
                && fetched_at.elapsed() < API_KEY_TTL
            {
                return Some(key.clone());
            }
        }

        let url =
            Url::parse(API_KEY_URL).unwrap_or_else(|e| panic!("the api-key URL must parse: {e}"));
        // `fetcher.text` errors are caught upstream → null → miss.
        let encrypted = ctx.fetcher.request(FetchRequest::get(url)).await.ok()?.body;
        let decrypted = decrypt_api_key(&encrypted)?;

        let mut cache = self
            .api_key_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *cache = Some((Instant::now(), decrypted.clone()));
        Some(decrypted)
    }
}

/// The September 2026 API exposes plaintext responses when its optional
/// `e=1` browser obfuscation flag is omitted. Keep old embeds compatible while
/// preferring the live endpoint over the removed `/api-key` route.
async fn current_streams(ctx: &ResolveCtx<'_>, parsed: &ParsedEmbed) -> Vec<Stream> {
    let Some(id) = parsed.tmdb_id.as_deref() else {
        return Vec::new();
    };
    let path = match (&parsed.season, &parsed.episode) {
        (Some(season), Some(episode)) => format!("tv/{id}/{season}/{episode}"),
        _ => format!("movie/{id}"),
    };
    let jobs = ["v4:English", "ipcloud", "dcloud", "tik"]
        .into_iter()
        .map(|server| {
            let path = &path;
            async move {
                let mut url =
                    Url::parse(&format!("https://core.vidzee.wtf/streams/{path}")).ok()?;
                url.query_pairs_mut().append_pair("s", server);
                let request = FetchRequest::get(url)
                    .with_header("Referer", "https://player.vidzee.wtf/")
                    .with_header("Origin", "https://player.vidzee.wtf")
                    .with_timeout(Duration::from_secs(5));
                let response = ctx.fetcher.request(request).await.ok()?;
                if !response.is_success() {
                    return None;
                }
                let data: Value = response.json().ok()?;
                let url = Url::parse(data.get("url")?.as_str()?).ok()?;
                if !matches!(url.scheme(), "http" | "https") {
                    return None;
                }
                let format = crate::helpers::format_for_url(&url);
                let mut stream = Stream::new(url, format)
                    .with_ttl(Duration::from_mins(5))
                    .with_label(format!("VidZee · {server}"));
                if let Some(headers) = data.get("headers").and_then(Value::as_object) {
                    stream.meta.request_headers = headers
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect();
                }
                // Browser playback supplies its embedding Referer implicitly.
                // Native players need it explicitly when the API omits it.
                if !stream
                    .meta
                    .request_headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("referer"))
                {
                    stream
                        .meta
                        .request_headers
                        .insert("Referer".into(), "https://player.vidzee.wtf/".into());
                }
                Some(stream)
            }
        });
    let results = futures::future::join_all(jobs).await;
    let mut seen = std::collections::HashSet::new();
    results
        .into_iter()
        .flatten()
        .filter(|s| seen.insert(s.url.clone()))
        .collect()
}

/// The embed URL's routing parts, ports `parseUrl`.
struct ParsedEmbed {
    /// The tmdb id following the `movie`/`tv` path segment.
    tmdb_id: Option<String>,
    /// The season (tv embeds only).
    season: Option<String>,
    /// The episode (tv embeds only).
    episode: Option<String>,
    /// The `?sr=` server id, defaulting to `4`.
    server_id: String,
}

impl ParsedEmbed {
    /// Split the embed path and query apart.
    fn from_url(url: &Url) -> Self {
        // `pathname.split('/').filter(Boolean)`.
        let parts: Vec<&str> = url
            .path()
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let mut tmdb_id = None;
        let mut season = None;
        let mut episode = None;
        if let Some(index) = parts.iter().position(|part| *part == "movie")
            && parts.get(index + 1).is_some()
        {
            tmdb_id = Some(parts[index + 1].to_string());
        } else if let Some(index) = parts.iter().position(|part| *part == "tv")
            && parts.get(index + 1).is_some()
        {
            tmdb_id = Some(parts[index + 1].to_string());
            season = parts.get(index + 2).map(|part| (*part).to_string());
            episode = parts.get(index + 3).map(|part| (*part).to_string());
        }
        // `searchParams.get('sr') ?? '4'`.
        let server_id = url
            .query_pairs()
            .find(|(key, _)| key.as_ref() == "sr")
            .map_or_else(|| "4".to_string(), |(_, value)| value.into_owned());
        Self {
            tmdb_id,
            season,
            episode,
            server_id,
        }
    }
}

/// `serverResponse.headers['User-Agent']`, when the API provides one.
fn server_user_agent(server: &Value) -> Option<&str> {
    server
        .get("headers")
        .and_then(|headers| str_field(headers, "User-Agent"))
}

/// Decrypt the base64 API-key response — ports `decryptApiKey`:
/// AES-256-GCM over `sha256(SEED)` with the payload `iv ‖ tag ‖ ct`.
///
/// Length errors, auth failures, and non-UTF-8 output all fail closed,
/// matching the upstream's thrown-and-caught errors.
fn decrypt_api_key(encrypted: &str) -> Option<String> {
    // 'Invalid API key response: too short' (≤ 28 leaves no ciphertext).
    let encrypted = decode_base64(encrypted)?;
    if encrypted.len() <= 28 {
        return None;
    }
    let (iv, rest) = encrypted.split_at(12);
    let (auth_tag, ciphertext) = rest.split_at(16);
    let key = Sha256::digest(ENCRYPTION_KEY_SECRET.as_bytes());
    let plaintext = gcm::decrypt(&key, iv, auth_tag, ciphertext)?;
    String::from_utf8(plaintext).ok()
}

/// Decrypt a server `link` — ports `decryptServerUrl`: the outer base64
/// decodes to `iv:ct` (both base64), then AES-256-CBC with the API key
/// zero-padded into a 32-byte buffer (keys longer than 32 bytes truncate,
/// exactly like `Buffer#write`).
fn decrypt_server_url(link: &str, api_key: &str) -> Option<String> {
    let decoded = String::from_utf8(decode_base64(link)?).ok()?;
    // `decoded.indexOf(':')`.
    let colon = decoded.find(':')?;
    let (iv_base64, ciphertext_base64) = (&decoded[..colon], &decoded[colon + 1..]);
    if iv_base64.is_empty() || ciphertext_base64.is_empty() {
        return None;
    }

    let iv = decode_base64(iv_base64)?;
    let ciphertext = decode_base64(ciphertext_base64)?;
    let mut key = [0u8; 32];
    let written = api_key.len().min(key.len());
    key[..written].copy_from_slice(&api_key.as_bytes()[..written]);

    let decryptor = cbc::Decryptor::<aes::Aes256>::new(
        GenericArray::from_slice(&key),
        GenericArray::from_slice(&iv),
    );
    let mut buffer = ciphertext;
    let plaintext = decryptor.decrypt_padded_mut::<Pkcs7>(&mut buffer).ok()?;
    String::from_utf8(plaintext.to_vec()).ok()
}

/// Ports `guessHeightFromPlaylist` from `src/utils/height.js`: the max
/// `WxH`/`NNNp` height advertised in the playlist. Best-effort — errors
/// map to `None` like the upstream try/catch.
async fn guess_height_from_playlist(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    headers: &BTreeMap<String, String>,
) -> Option<u16> {
    let mut request = FetchRequest::get(url.clone());
    for (name, value) in headers {
        request = request.with_header(name, value);
    }
    let playlist = ctx.fetcher.request(request).await.ok()?.body;
    let mut best: Option<u16> = None;
    for captures in HEIGHT.captures_iter(&playlist).flatten() {
        let height = captures
            .get(1)
            .or_else(|| captures.get(2))
            .and_then(|group| group.as_str().parse::<u16>().ok());
        if let Some(height) = height {
            best = Some(best.map_or(height, |current: u16| current.max(height)));
        }
    }
    best
}

/// Base64 decoding as lenient as Node's `Buffer.from(…, 'base64')`:
/// whitespace skipped, urlsafe alphabet mapped, padding optional.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let filtered: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let standard = filtered.replace('-', "+").replace('_', "/");
    let unpadded = standard.trim_end_matches('=');
    STANDARD_NO_PAD.decode(unpadded).ok()
}

/// `value?.key` as a borrowed string.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// AES-256-GCM decryption over the raw `aes` block cipher.
///
/// The upstream decrypts the API key with `node:crypto`'s
/// `aes-256-gcm`; the workspace ships no GCM crate, so this is a minimal
/// SP 800-38D decrypt-and-verify (empty associated data) for exactly the
/// payload shape the site returns.
mod gcm {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::{Aes256, Block};

    /// The GHASH reduction constant `R = E1 ‖ 0^120`.
    const R: u128 = 0xE1 << 120;

    /// Decrypt `ciphertext` under `key`, verifying `auth_tag`.
    ///
    /// Returns `None` on length errors or tag mismatch — the upstream
    /// `decipher.final()` throw.
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

        // CTR keystream from inc32(J0); the first data block uses J0 + 1.
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
    /// over the ciphertext blocks (zero-padded) and the bit-length block.
    fn ghash(h: [u8; 16], data: &[u8]) -> [u8; 16] {
        let h = u128::from_be_bytes(h);
        let mut y: u128 = 0;
        let mut block = [0u8; 16];
        for chunk in data.chunks(16) {
            block.fill(0);
            block[..chunk.len()].copy_from_slice(chunk);
            y = gmul(y ^ u128::from_be_bytes(block), h);
        }
        // [len(AAD) = 0 (bits) ‖ len(data) (bits)], both big-endian.
        block.fill(0);
        block[8..].copy_from_slice(&u64::try_from(data.len() * 8).unwrap_or(0).to_be_bytes());
        y = gmul(y ^ u128::from_be_bytes(block), h);
        y.to_be_bytes()
    }

    /// Multiplication in GF(2^128) with the GCM reduction polynomial —
    /// the shift-and-XOR construction of SP 800-38D (block bits
    /// big-endian).
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

    /// `inc32`: increment the big-endian last 32 bits of the block.
    fn increment_counter(counter: &mut [u8; 16]) {
        let value = u32::from_be_bytes([counter[12], counter[13], counter[14], counter[15]]);
        let incremented = value.wrapping_add(1);
        counter[12..].copy_from_slice(&incremented.to_be_bytes());
    }

    fn xor(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
        (u128::from_be_bytes(*a) ^ u128::from_be_bytes(*b)).to_be_bytes()
    }

    /// A constant-time comparison of the computed and expected tags.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::STANDARD;

    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    /// A known plaintext/ciphertext pair for the API-key payload,
    /// generated with `node:crypto` during the port: AES-256-GCM under
    /// `sha256(SEED)` with iv `1a2b3c4d5e6f708192a3b4c5`, 31 bytes of
    /// ciphertext (a partial final block).
    const API_KEY_PAYLOAD: &str =
        "Gis8TV5vcIGSo7TFpZz3IPbsihjf4b+vWD/05Dw/2x6t9bl4OlFO2v+UFiI9wH5sS/oFU/XaEPSO4TY=";
    /// The vector's plaintext — the rotating site key.
    const DECRYPTED_API_KEY: &str = "JH5s2K9vQx7Lm3Np8Rt6Wz4Yb1Cd0Ef";

    /// A known plaintext/ciphertext pair for a server `link`, generated
    /// with `node:crypto` during the port: AES-256-CBC under
    /// `DECRYPTED_API_KEY` (zero-padded to 32 bytes), iv
    /// `00112233445566778899aabbccddeeff`.
    const SERVER_LINK: &str = "QUJFaU0wUlZabmVJbWFxN3pOM3Uvdz09Okl3TzJDRkkzUkhzVm9kUy9XbG5ESUcyYjZkTkFMbHlUWlU2eE5wS254Z2xlQTF1dERKM1FGSXpqYXBJdW9xRFQ=";
    const SERVER_LINK_URL: &str = "https://edge.vidzee.wtf/hls/12345/master.m3u8";

    /// The `/api/server` response: one hls entry plus the site-specified
    /// hotlink `User-Agent`.
    const SERVER_API_JSON: &str = r#"{
        "url": [
            {
                "name": "Vidzee Cloud",
                "flag": "US",
                "lang": "English",
                "type": "hls",
                "link": "QUJFaU0wUlZabmVJbWFxN3pOM3Uvdz09Okl3TzJDRkkzUkhzVm9kUy9XbG5ESUcyYjZkTkFMbHlUWlU2eE5wS254Z2xlQTF1dERKM1FGSXpqYXBJdW9xRFQ="
            }
        ],
        "headers": {"User-Agent": "Mozilla/5.0 (Windows NT 10.0) Chrome/131"}
    }"#;
    const SITE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0) Chrome/131";

    fn movie_url() -> Url {
        Url::parse("https://player.vidzee.wtf/embed/movie/12345")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    fn vidzee_fixtures() -> ScriptedFetcher {
        ScriptedFetcher::default()
            .page("/api-key", API_KEY_PAYLOAD)
            .page("/api/server", SERVER_API_JSON)
            .page(
                "/hls/12345/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080\n1080.m3u8\n",
            )
    }

    fn server_query(fetcher: &ScriptedFetcher) -> String {
        fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/api/server")
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn decrypts_the_api_key_payload() {
        assert_eq!(
            decrypt_api_key(API_KEY_PAYLOAD).as_deref(),
            Some(DECRYPTED_API_KEY)
        );
        // Too-short payloads fail closed.
        assert!(decrypt_api_key("AAAA").is_none());
        // A tampered tag fails the auth check.
        let mut corrupted = STANDARD
            .decode(API_KEY_PAYLOAD)
            .unwrap_or_else(|e| panic!("the vector must decode: {e}"));
        corrupted[13] ^= 0xFF;
        let corrupted = STANDARD.encode(&corrupted);
        assert!(decrypt_api_key(&corrupted).is_none());
        // A payload under the wrong seed fails the auth check.
        let payload = STANDARD
            .decode(API_KEY_PAYLOAD)
            .unwrap_or_else(|e| panic!("the vector must decode: {e}"));
        let (iv, rest) = payload.split_at(12);
        let (tag, ciphertext) = rest.split_at(16);
        let wrong_seed = Sha256::digest(b"not-the-seed");
        assert!(gcm::decrypt(&wrong_seed, iv, tag, ciphertext).is_none());
    }

    #[test]
    fn decrypts_server_links() {
        assert_eq!(
            decrypt_server_url(SERVER_LINK, DECRYPTED_API_KEY).as_deref(),
            Some(SERVER_LINK_URL)
        );
        // Garbage links and wrong keys fail closed.
        assert!(decrypt_server_url("garbage", DECRYPTED_API_KEY).is_none());
        assert!(decrypt_server_url(SERVER_LINK, "00000000000000000000000000000000").is_none());
        // A short key is zero-padded to the 32-byte buffer, exactly like
        // `Buffer#write` into `Buffer.alloc(32)`.
        let short_key_link = encrypt_server_link("short-key", SERVER_LINK_URL);
        assert_eq!(
            decrypt_server_url(&short_key_link, "short-key").as_deref(),
            Some(SERVER_LINK_URL)
        );
    }

    /// CBC-encrypt `plaintext` under `key` into a `iv:ct` base64 link
    /// (the test-side inverse of `decrypt_server_url`).
    fn encrypt_server_link(key: &str, plaintext: &str) -> String {
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        type Cbc = cbc::Encryptor<aes::Aes256>;
        let mut key_bytes = [0u8; 32];
        let written = key.len().min(key_bytes.len());
        key_bytes[..written].copy_from_slice(&key.as_bytes()[..written]);
        let iv = [0u8; 16];
        let mut buffer = vec![0u8; plaintext.len() + 16];
        buffer[..plaintext.len()].copy_from_slice(plaintext.as_bytes());
        let encryptor = Cbc::new(
            GenericArray::from_slice(&key_bytes),
            GenericArray::from_slice(&iv),
        );
        let ciphertext = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .unwrap_or_else(|e| panic!("the test encryption must pad: {e}"))
            .to_vec();
        let link = format!("{}:{}", STANDARD.encode(iv), STANDARD.encode(ciphertext));
        STANDARD.encode(link)
    }

    #[test]
    fn matches_the_vidzee_hosts() {
        let extractor = VidZee::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/embed/movie/12345"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("player.vidzee.wtf"));
        assert!(supports("v2.vidzee.wtf"));
        assert!(!supports("vidzee.wtf"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn resolves_movie_embeds() {
        let fetcher = vidzee_fixtures();
        let ctx = ctx_for(&fetcher, None);

        let streams = VidZee::new()
            .extract(&ctx, &movie_url())
            .await
            .unwrap_or_else(|e| panic!("the movie embed must resolve: {e}"));
        assert_direct_stream(&streams, Format::Hls, SERVER_LINK_URL);
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL);
        assert_eq!(stream.label.as_deref(), Some("Vidzee Cloud (US) - English"));
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://player.vidzee.wtf/")
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(SITE_USER_AGENT)
        );
        // The API shape: id + the default sr (no ?sr= in the embed).
        assert_eq!(server_query(&fetcher), "id=12345&sr=4");
    }

    #[tokio::test]
    async fn resolves_tv_embeds_with_season_and_episode() {
        let fetcher = vidzee_fixtures();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://player.vidzee.wtf/embed/tv/67071/2/5")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = VidZee::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the tv embed must resolve: {e}"));
        assert_direct_stream(&streams, Format::Hls, SERVER_LINK_URL);
        // The API shape: id + sr + ss + ep for series.
        assert_eq!(server_query(&fetcher), "id=67071&sr=4&ss=2&ep=5");
    }

    #[tokio::test]
    async fn server_errors_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/api-key", API_KEY_PAYLOAD)
            .page("/api/server", r#"{"error":"not found"}"#);
        let ctx = ctx_for(&fetcher, None);

        match VidZee::new().extract(&ctx, &movie_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an API error must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embeds_without_a_tmdb_id_are_misses() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://player.vidzee.wtf/watch/whatever")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match VidZee::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an id-less embed must be a NotFound, got {other:?}"),
        }
        // No API call is made without an id.
        assert!(fetcher.requests().is_empty());
    }

    #[tokio::test]
    async fn undecryptable_api_keys_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/api-key", "not-a-real-payload")
            .page("/api/server", SERVER_API_JSON);
        let ctx = ctx_for(&fetcher, None);

        match VidZee::new().extract(&ctx, &movie_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a bad API key must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn caches_the_api_key_for_an_hour() {
        let fetcher = vidzee_fixtures();
        let ctx = ctx_for(&fetcher, None);
        let extractor = VidZee::new();

        for _ in 0..2 {
            let streams = extractor
                .extract(&ctx, &movie_url())
                .await
                .unwrap_or_else(|e| panic!("the embed must resolve: {e}"));
            assert_direct_stream(&streams, Format::Hls, SERVER_LINK_URL);
        }
        // Two extractions, one API-key fetch.
        let api_key_requests = fetcher
            .requests()
            .iter()
            .filter(|request| request.url.path() == "/api-key")
            .count();
        assert_eq!(api_key_requests, 1);
    }
    #[tokio::test]
    async fn current_api_resolves_without_the_removed_key_route() -> Result<(), ExtractorError> {
        let fetcher = ScriptedFetcher::default().page("/streams/movie/27205", r#"{"url":"https://cdn.example/master.m3u8","language":"English","headers":{"Referer":"https://origin.example/"}}"#);
        let ctx = ctx_for(&fetcher, None);
        let embed = Url::parse("https://player.vidzee.wtf/v2/embed/movie/27205?sr=4")
            .unwrap_or_else(|e| panic!("URL: {e}"));
        let extractor = VidZee::new();
        let streams = extractor.extract(&ctx, &embed).await?;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://origin.example/")
        );
        assert!(
            fetcher
                .requests()
                .iter()
                .all(|r| r.url.path() != "/api-key")
        );
        let calls = fetcher.requests().len();
        extractor.extract(&ctx, &embed).await?;
        assert_eq!(fetcher.requests().len(), calls);
        Ok(())
    }
    #[tokio::test]
    async fn native_playback_supplies_the_implicit_browser_referer() -> Result<(), ExtractorError> {
        for (headers, expected) in [
            (serde_json::json!({}), "https://player.vidzee.wtf/"),
            (
                serde_json::json!({"referer":"https://custom.example/"}),
                "https://custom.example/",
            ),
        ] {
            let body =
                serde_json::json!({"url":"https://cdn.example/master.m3u8","headers":headers})
                    .to_string();
            let fetcher = ScriptedFetcher::default().page("/streams/movie/27205", body);
            let ctx = ctx_for(&fetcher, None);
            let url = Url::parse("https://player.vidzee.wtf/embed/movie/27205")
                .unwrap_or_else(|e| panic!("URL: {e}"));
            let streams = VidZee::new().extract(&ctx, &url).await?;
            let refs: Vec<_> = streams[0]
                .meta
                .request_headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("referer"))
                .map(|(_, v)| v.as_str())
                .collect();
            assert_eq!(refs, vec![expected]);
        }
        Ok(())
    }
}
