//! `IMDBPlay`: the vidsrc.me backend chain, decrypted in pure Rust.
//!
//! Ports `src/source/IMDBPlay.js` (`imdbplay.tech` — movies/TV/anime
//! up to 4K through the vidsrc.me backend). The upstream
//! reverse-engineered chain, resolved end-to-end to tokened HLS
//! masters:
//!
//! 1. TMDB → `IMDb` id.
//! 2. `proxy.garageband.rocks/vs_src.php?type=…&id={imdb}[&season&episode]`
//!    → `{src: embed URL}`.
//! 3. The embed page is fetched only for its origin (upstream called it
//!    "for metaApi" but the data API URL is fixed) — the origin becomes
//!    the `Referer` for the next hop.
//! 4. `data.vidsrcme.ru/api.php?type=…&imdb={imdb}&stream_urls` →
//!    `{data: {file_name, stream_urls}, vs: {w, wasm_url}}` — the
//!    quality/codec/source/language labels all come from `file_name`.
//! 5. The `stream_urls` blob (base64) is decrypted with the key material
//!    of the WASM module served at `vs.wasm_url`: the module is a
//!    modified `ChaCha20` whose state is `[constants, mask ⊕ key, counter,
//!    nonce]`, where the mask and key are the wasm's two 32-byte data
//!    segments and the nonce is the blob's 12-byte prefix (ciphertext
//!    follows). The port **parses the wasm instead of executing it**:
//!    section scan → data segments → the two 32-byte segments → XOR.
//!    Verified byte-for-byte against the live module (see the
//!    ground-truth test).
//! 6. Each stream host's `generate.php` (with
//!    `Referer: https://cloudorchestranova.com/`) mints an IP-bound JWT
//!    appended as `?token=…` — one stream per host.
//!
//! Cuts and constraints for the library port:
//!
//! - The upstream routed the final URL through its `/proxy` for HLS
//!   rewriting (the JWT binds the minting /24); there is no server
//!   here, so the stream ships with
//!   `Referer: {embed origin}` in
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!   — the same header the proxy carried.
//! - Binary WASM is fetched through the bounded raw-byte transport, preserving
//!   its embedded key bytes. Custom fetchers need `Fetcher::probe` support.
//! - The upstream `got` http2/GOAWAY retry logic is the net layer's
//!   concern; `meta.serverName` and `isMultiAudio` have no
//!   `StreamMeta` fields.
//! - The upstream's TMDB genre/original-language anime probe
//!   (`isAnimeContent`) is cut: the shared `TmdbClient` does not expose
//!   genres, so language labels always use the non-anime mapping
//!   (`Dual Audio`/`Hindi`/`English`, default `English`) and the
//!   languages are the non-anime `multi/hi/en` set.
//! - `meta.title` has no `StreamMeta` field — the stream label carries
//!   the `${title} — [IMDBPlay ${height}p ${sourceType} ${codec}
//!   ${language}]` form.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

/// The garageband embed resolver.
const GARAGEBAND_API: &str = "https://proxy.garageband.rocks/vs_src.php";
/// The vidsrc.me data API.
const VS_API: &str = "https://data.vidsrcme.ru/api.php";
/// The site prefix for both.
const GARAGEBAND_REFERER: &str = "https://proxy.garageband.rocks/";
/// The token mint's Referer (the embed family's shared origin).
const TOKEN_REFERER: &str = "https://cloudorchestranova.com/";
/// Upstream's default request timeout (`gotGet`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// The WASM download timeout (upstream: 10s).
const WASM_TIMEOUT: Duration = Duration::from_secs(10);
/// The token mint timeout (upstream: 8s).
const TOKEN_TIMEOUT: Duration = Duration::from_secs(8);
/// Upstream `this.ttl` — tokens are short-lived.
const TTL: Duration = Duration::from_mins(5);

/// The `ChaCha20` `expand 32-byte k` constants.
const CHACHA_CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// The `IMDBPlay` provider.
pub struct IMDBPlay {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl IMDBPlay {
    /// A provider over the shared TMDB client.
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "imdbplay".to_string(),
                label: "IMDBPlay".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En],
                base_url: Some(
                    Url::parse("https://www.imdbplay.tech")
                        .unwrap_or_else(|e| panic!("valid IMDBPlay base URL: {e}")),
                ),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// Step 1 — the garageband embed resolver (`vs_src.php`).
    async fn embed_url(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        imdb_id: &str,
    ) -> Result<Url, SourceError> {
        let media_type = if media.season.is_some() {
            "tv"
        } else {
            "movie"
        };
        let mut target = format!("{GARAGEBAND_API}?type={media_type}&id={imdb_id}");
        if let (Some(season), Some(episode)) = (media.season, media.episode) {
            let _ = write!(target, "&season={season}&episode={episode}");
        }
        let url = Url::parse(&target)
            .map_err(|_| SourceError::scrape("imdbplay", "invalid vs_src.php URL"))?;
        let request = FetchRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("Referer", GARAGEBAND_REFERER)
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx
            .fetcher
            .request(request)
            .await
            .map_err(|_| SourceError::NotFound)?;
        if !response.is_success() {
            return Err(SourceError::NotFound);
        }
        let payload: VsSrcResponse = response.json().map_err(|_| SourceError::NotFound)?;
        let src = payload
            .src
            .filter(|src| !src.is_empty())
            .ok_or(SourceError::NotFound)?;
        // Upstream's unguarded `new URL(embedUrl)` — a malformed src is a
        // structural surprise.
        Url::parse(&src).map_err(|_| SourceError::scrape("imdbplay", "malformed embed URL"))
    }

    /// Step 2 — the embed page, whose origin is the data API's Referer.
    async fn embed_origin(
        &self,
        ctx: &ResolveCtx<'_>,
        embed_url: &Url,
    ) -> Result<String, SourceError> {
        let request = FetchRequest::get(embed_url.clone())
            .with_header("Referer", GARAGEBAND_REFERER)
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx
            .fetcher
            .request(request)
            .await
            .map_err(|_| SourceError::NotFound)?;
        if !response.is_success() {
            return Err(SourceError::NotFound);
        }
        Ok(format!(
            "{}://{}/",
            embed_url.scheme(),
            embed_url.host_str().unwrap_or_default()
        ))
    }

    /// Steps 3-5 — the data API (file name and encrypted URLs in ONE
    /// payload, as upstream), the WASM decrypt, and the token mints.
    async fn stream_urls(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        imdb_id: &str,
        embed_origin: &str,
    ) -> Result<(Vec<Url>, String), SourceError> {
        let media_type = if media.season.is_some() {
            "tv"
        } else {
            "movie"
        };
        let mut target = format!("{VS_API}?type={media_type}&imdb={imdb_id}&stream_urls");
        if let (Some(season), Some(episode)) = (media.season, media.episode) {
            let _ = write!(target, "&season={season}&episode={episode}");
        }
        let url = Url::parse(&target)
            .map_err(|_| SourceError::scrape("imdbplay", "invalid api.php URL"))?;
        let request = FetchRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("Referer", embed_origin)
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx
            .fetcher
            .request(request)
            .await
            .map_err(|_| SourceError::NotFound)?;
        if !response.is_success() {
            return Err(SourceError::NotFound);
        }
        let payload: ApiPayload = response.json().map_err(|_| SourceError::NotFound)?;
        let data = payload.data.ok_or(SourceError::NotFound)?;
        let file_name = data.file_name.unwrap_or_default();
        let stream_urls = data.stream_urls.ok_or(SourceError::NotFound)?;
        let wasm_url = payload
            .vs
            .and_then(|vs| vs.wasm_url)
            .filter(|url| !url.is_empty())
            .ok_or(SourceError::NotFound)?;

        // Preserve the exact WASM bytes; a text conversion corrupts its
        // embedded key material before the parser ever sees it.
        let wasm_url = Url::parse(&wasm_url)
            .map_err(|_| SourceError::scrape("imdbplay", "malformed wasm_url"))?;
        let wasm_request = FetchRequest::get(wasm_url)
            .with_header("Accept", "*/*")
            .with_timeout(WASM_TIMEOUT);
        let wasm_response = ctx
            .fetcher
            .probe(wasm_request, 2 * 1024 * 1024)
            .await
            .map_err(|_| SourceError::NotFound)?
            .ok_or(SourceError::NotFound)?;
        if !(200..300).contains(&wasm_response.status) || wasm_response.truncated {
            return Err(SourceError::NotFound);
        }
        let decrypted = decrypt_stream_urls_bytes(&stream_urls, &wasm_response.body)
            .ok_or(SourceError::NotFound)?;

        // Step 5: mint a token per stream host.
        let mut urls = Vec::new();
        for stream_url in decrypted {
            let Ok(parsed) = Url::parse(&stream_url) else {
                // Upstream caught per-stream and skipped.
                continue;
            };
            let Some(host) = parsed.host_str() else {
                continue;
            };
            let Some(token) = fetch_token(ctx, host).await else {
                continue;
            };
            let separator = if parsed.query().is_some() { '&' } else { '?' };
            let tokened = format!("{stream_url}{separator}token={token}");
            if let Ok(url) = Url::parse(&tokened) {
                urls.push(url);
            }
        }
        Ok((urls, file_name))
    }
}

#[async_trait]
impl Source for IMDBPlay {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(&self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let imdb_id = imdb_id(ctx, &self.tmdb, media, tmdb_id).await?;
        // Upstream requires a `tt…` id; anything else is a miss.
        let Some(imdb) = imdb_id.filter(|id| id.starts_with("tt")) else {
            return Err(SourceError::NotFound);
        };

        let embed_url = self.embed_url(ctx, media, &imdb).await?;
        let embed_origin = self.embed_origin(ctx, &embed_url).await?;
        let (urls, file_name) = self.stream_urls(ctx, media, &imdb, &embed_origin).await?;
        if urls.is_empty() {
            return Err(SourceError::NotFound);
        }

        let title = display_title(&name, year, media.season, media.episode);
        let height = parse_height(&file_name);
        let codec = parse_codec(&file_name);
        let source_type = parse_source_type(&file_name);
        let language = parse_language(&file_name);
        let label = format!("{title} — [IMDBPlay {height}p {source_type} {codec} {language}]");

        Ok(urls
            .into_iter()
            .map(|url| {
                let mut stream = Stream::new(url.clone(), Format::Hls)
                    .with_ttl(TTL)
                    .with_referer(embed_origin.clone());
                stream.label = Some(label.clone());
                stream.meta.resolution = Some(height);
                stream.meta.quality = Some(source_type.to_string());
                stream.meta.codec = Some(codec.to_string());
                stream.meta.audio = vec![language.to_string()];
                stream.meta.languages = vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En];
                stream.meta.source_id = Some("imdbplay".to_string());
                stream.meta.source_label = Some("IMDBPlay".to_string());
                stream
            })
            .collect())
    }
}

/// The `vs_src.php` response.
#[derive(Deserialize)]
struct VsSrcResponse {
    /// The embed page URL.
    #[serde(default)]
    src: Option<String>,
}

/// The `api.php` response.
#[derive(Deserialize)]
struct ApiPayload {
    /// The media payload.
    #[serde(default)]
    data: Option<ApiData>,
    /// The WASM metadata.
    #[serde(default)]
    vs: Option<VsMeta>,
}

/// The media payload.
#[derive(Deserialize)]
struct ApiData {
    /// The release file name (quality/codec/source hints).
    #[serde(rename = "file_name", default)]
    file_name: Option<String>,
    /// The encrypted stream URL list.
    #[serde(rename = "stream_urls", default)]
    stream_urls: Option<String>,
}

/// The WASM metadata.
#[derive(Deserialize)]
struct VsMeta {
    /// The per-request WASM module URL.
    #[serde(rename = "wasm_url", default)]
    wasm_url: Option<String>,
}

/// Mint the playback token from the stream host's `generate.php`
/// (upstream `fetchToken`): 200, a non-HTML body, trimmed.
async fn fetch_token(ctx: &ResolveCtx<'_>, host: &str) -> Option<String> {
    let url = Url::parse(&format!("https://{host}/generate.php")).ok()?;
    let request = FetchRequest::get(url)
        .with_header("Referer", TOKEN_REFERER)
        .with_timeout(TOKEN_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let body = response.body.trim();
    (response.is_success() && !body.is_empty() && !body.contains('<')).then(|| body.to_string())
}

/// The height label in the file name (upstream `parseHeight`, default
/// 1080).
fn parse_height(text: &str) -> u16 {
    let lower = text.to_lowercase();
    if lower.contains("2160") || lower.contains("4k") {
        2160
    } else if lower.contains("1080") {
        1080
    } else if lower.contains("720") {
        720
    } else if lower.contains("480") {
        480
    } else {
        1080
    }
}

/// The codec family in the file name (upstream `parseCodec`, default
/// `x264`).
fn parse_codec(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.contains("x265")
        || lower.contains("h265")
        || lower.contains("hevc")
        || lower.contains("h.265")
    {
        "HEVC"
    } else if lower.contains("x264")
        || lower.contains("h264")
        || lower.contains("h.264")
        || lower.contains("avc")
    {
        "x264"
    } else if lower.contains("2160") || lower.contains("4k") {
        "HEVC"
    } else {
        "x264"
    }
}

/// The release source label (upstream `parseSourceType`, default
/// `WebDL`).
fn parse_source_type(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.contains("remux") {
        "BluRay Remux"
    } else if lower.contains("bluray") || lower.contains("brrip") || lower.contains("bdrip") {
        "BluRay"
    } else if lower.contains("web-dl") || lower.contains("webdl") || lower.contains("web dl") {
        "WebDL"
    } else if lower.contains("webrip") {
        "WebRip"
    } else if lower.contains("hdrip") {
        "HDRip"
    } else {
        "WebDL"
    }
}

/// The audio language label (upstream `parseLanguage` with the anime
/// branch cut, default `English`).
fn parse_language(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.contains("dual") || (lower.contains("hindi") && lower.contains("english")) {
        "Dual Audio"
    } else if lower.contains("hindi") {
        "Hindi"
    } else {
        "English"
    }
}

/// Decrypt the `stream_urls` blob with the WASM module's key material:
/// `nonce = blob[0..12]`, `ciphertext = blob[12..]`, keystream from the
/// modified `ChaCha20`. `None` on any failure (bad base64, unparseable
/// wasm, non-UTF-8 plaintext).
fn decrypt_stream_urls_bytes(encrypted: &str, wasm_body: &[u8]) -> Option<Vec<String>> {
    let blob = base64_decode(encrypted)?;
    if blob.len() < 12 {
        return None;
    }
    let (nonce, ciphertext) = blob.split_at(12);
    let key_words = wasm_key_words(wasm_body)?;
    let state = key_state(&key_words, nonce);
    let keystream = keystream_bytes(&state, ciphertext.len());
    let plaintext: Vec<u8> = ciphertext
        .iter()
        .zip(keystream)
        .map(|(byte, key)| byte ^ key)
        .collect();
    let text = String::from_utf8(plaintext).ok()?;
    Some(
        text.lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The `ChaCha` state: constants, `mask ⊕ key` words, counter (0), nonce.
fn key_state(key_words: &[u32; 8], nonce: &[u8]) -> [u32; 16] {
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&CHACHA_CONSTANTS);
    state[4..12].copy_from_slice(key_words);
    state[13] = le32(nonce, 0);
    state[14] = le32(nonce, 4);
    state[15] = le32(nonce, 8);
    state
}

/// The keystream of `length` bytes for a state.
fn keystream_bytes(state: &[u32; 16], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let blocks = length.div_ceil(64);
    for block in 0..blocks {
        let counter = u32::try_from(block).unwrap_or(u32::MAX);
        out.extend_from_slice(&chacha20_block(state, counter));
    }
    out.truncate(length);
    out
}

/// One 64-byte `ChaCha20` block (20 rounds, counter in word 12) — the
/// exact function the WASM exports as `decrypt`'s core.
fn chacha20_block(state: &[u32; 16], counter: u32) -> [u8; 64] {
    let mut x = *state;
    x[12] = counter;
    let input = x;
    for _ in 0..10 {
        // Column rounds.
        quarter_round(&mut x, 0, 4, 8, 12);
        quarter_round(&mut x, 1, 5, 9, 13);
        quarter_round(&mut x, 2, 6, 10, 14);
        quarter_round(&mut x, 3, 7, 11, 15);
        // Diagonal rounds.
        quarter_round(&mut x, 0, 5, 10, 15);
        quarter_round(&mut x, 1, 6, 11, 12);
        quarter_round(&mut x, 2, 7, 8, 13);
        quarter_round(&mut x, 3, 4, 9, 14);
    }
    let mut block = [0u8; 64];
    for (index, word) in x.iter().enumerate() {
        block[index * 4..index * 4 + 4]
            .copy_from_slice(&word.wrapping_add(input[index]).to_le_bytes());
    }
    block
}

/// The `ChaCha20` quarter round on four state words.
// The a/b/c/d names mirror the ChaCha quarter-round notation.
#[allow(clippy::many_single_char_names)]
fn quarter_round(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] ^= x[a];
    x[d] = x[d].rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] ^= x[c];
    x[b] = x[b].rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] ^= x[a];
    x[d] = x[d].rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] ^= x[c];
    x[b] = x[b].rotate_left(7);
}

/// The XOR-combined key words from the wasm's two 32-byte data
/// segments (the mask and the key — the cipher only ever uses their
/// XOR, so the pairing order does not matter).
fn wasm_key_words(wasm_body: &[u8]) -> Option<[u32; 8]> {
    let segments = wasm_data_segments(wasm_body)?;
    let words: Vec<&[u8]> = segments
        .iter()
        .map(Vec::as_slice)
        .filter(|segment| segment.len() == 32)
        .collect();
    // Exactly two 32-byte segments carry the key material; more means
    // decoys the parser cannot disambiguate.
    if words.len() != 2 {
        return None;
    }
    let mut key = [0u32; 8];
    for (index, word) in key.iter_mut().enumerate() {
        *word = le32(words[0], index * 4) ^ le32(words[1], index * 4);
    }
    Some(key)
}

/// Every data segment of the wasm's data section (section id 11),
/// scanning the section table without validating the rest of the
/// module.
fn wasm_data_segments(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return None;
    }
    let mut cursor = 8;
    let mut segments = Vec::new();
    while cursor < bytes.len() {
        let section_id = *bytes.get(cursor)?;
        cursor += 1;
        let size = read_leb(bytes, &mut cursor)? as usize;
        let end = cursor.checked_add(size)?;
        let payload = bytes.get(cursor..end)?;
        if section_id == 11 {
            segments.extend(parse_data_section(payload)?);
        }
        cursor = end;
    }
    Some(segments)
}

/// The data section's segments: `(memidx 0, i32.const offset, end,
/// size, bytes)` each.
fn parse_data_section(payload: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut cursor = 0;
    let count = read_leb(payload, &mut cursor)?;
    let mut segments = Vec::new();
    for _ in 0..count {
        if payload.get(cursor) != Some(&0) {
            return None;
        }
        cursor += 1;
        if payload.get(cursor) != Some(&0x41) {
            return None;
        }
        cursor += 1;
        let _offset = read_leb(payload, &mut cursor)?;
        if payload.get(cursor) != Some(&0x0b) {
            return None;
        }
        cursor += 1;
        let size = read_leb(payload, &mut cursor)? as usize;
        let end = cursor.checked_add(size)?;
        segments.push(payload.get(cursor..end)?.to_vec());
        cursor = end;
    }
    Some(segments)
}

/// A little-endian LEB128 u32 at `*cursor`, advancing it.
fn read_leb(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// The little-endian u32 at `offset`.
fn le32(bytes: &[u8], offset: usize) -> u32 {
    let byte = |k: usize| u32::from(*bytes.get(offset + k).unwrap_or(&0));
    byte(0) | byte(1) << 8 | byte(2) << 16 | byte(3) << 24
}

/// Standard base64 decoding (padding and whitespace tolerated).
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in text.chars() {
        if ch == '=' || ch == '\n' || ch == '\r' {
            continue;
        }
        let value = match ch {
            'A'..='Z' => u32::from(ch) - u32::from('A'),
            'a'..='z' => u32::from(ch) - u32::from('a') + 26,
            '0'..='9' => u32::from(ch) - u32::from('0') + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// The TMDB id for the reference (IMDb-keyed references resolve through
/// `/find`); TMDB miss pages map to [`SourceError::NotFound`] like the
/// upstream `NotFoundError`.
async fn tmdb_id(tmdb: &TmdbClient, media: &MediaRef) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
    }
}

/// The media name and year, preferring pre-resolved context metadata
/// (the upstream resolver resolved TMDB before calling sources) and
/// falling back to the shared client.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = &ctx.media
        && !resolved.name.is_empty()
    {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
    Ok((name.name, name.year))
}

/// The `IMDb` id for the reference — the id itself when IMDb-keyed, the
/// pre-resolved metadata's id when present, `external_ids` otherwise.
async fn imdb_id(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<Option<String>, SourceError> {
    if let Some(imdb) = media.id.as_imdb() {
        return Ok(Some(imdb.to_string()));
    }
    if let Some(resolved) = &ctx.media
        && let Some(imdb) = &resolved.imdb_id
    {
        return Ok(Some(imdb.clone()));
    }
    soften(tmdb.imdb_id_from_tmdb(tmdb_id, media.kind).await)
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

/// The upstream display title: name + `S01E02` for episodes, name +
/// ` (year)` for movies.
fn display_title(
    name: &str,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
) -> String {
    if season.is_some() {
        format!(
            "{name} S{:02}E{:02}",
            season.unwrap_or(1),
            episode.unwrap_or(1)
        )
    } else {
        let year = year.map(|y| y.to_string()).unwrap_or_default();
        format!("{name} ({year})")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;
    fn decrypt_stream_urls(encrypted: &str, wasm_body: &str) -> Option<Vec<String>> {
        let bytes: Vec<u8> = wasm_body
            .chars()
            .map(|ch| u8::try_from(u32::from(ch)).ok())
            .collect::<Option<_>>()?;
        decrypt_stream_urls_bytes(encrypted, &bytes)
    }

    use vsources_core::traits::{FetchResponse, Fetcher};

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        status: u16,
        body: String,
        headers: BTreeMap<String, String>,
    }

    impl Scripted {
        /// A 200 JSON body.
        fn json(value: &serde_json::Value) -> Self {
            Self {
                status: 200,
                body: value.to_string(),
                headers: BTreeMap::from([(
                    "content-type".to_string(),
                    "application/json".to_string(),
                )]),
            }
        }

        /// A 200 text body.
        fn text(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                body: body.into(),
                headers: BTreeMap::from([("content-type".to_string(), "text/plain".to_string())]),
            }
        }
    }

    /// A fetcher serving scripted pages by host+path (in order, the last
    /// repeating) and recording every request it sees.
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

        /// Serve `key` (host + path) with `response`.
        fn serve(self, key: &str, response: Scripted) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.to_string())
                .or_default()
                .push_back(response);
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The value of a header sent to `key`.
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| {
                    let host = request.url.host_str().unwrap_or_default();
                    format!("{host}{}", request.url.path()) == key
                })
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(header, _)| header.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }

        /// The query string of the first request to `key`.
        fn query_of(&self, key: &str) -> String {
            self.requests()
                .iter()
                .find(|request| {
                    let host = request.url.host_str().unwrap_or_default();
                    format!("{host}{}", request.url.path()) == key
                })
                .map(|request| request.url.query().unwrap_or_default().to_string())
                .unwrap_or_default()
        }
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn probe(
            &self,
            request: FetchRequest,
            max_bytes: usize,
        ) -> Result<Option<vsources_core::traits::ProbeResponse>, FetchError> {
            let response = self.request(request).await?;
            let Some(mut body) = response
                .body
                .chars()
                .map(|ch| u8::try_from(u32::from(ch)).ok())
                .collect::<Option<Vec<u8>>>()
            else {
                return Ok(None);
            };
            let truncated = body.len() >= max_bytes;
            body.truncate(max_bytes);
            Ok(Some(vsources_core::traits::ProbeResponse {
                url: response.url,
                status: response.status,
                headers: response.headers,
                body,
                truncated,
            }))
        }

        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let key = format!(
                "{}{}",
                request.url.host_str().unwrap_or_default(),
                request.url.path()
            );
            let mut pages = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let response = pages.get_mut(&key).map(|queue| {
                // The last scripted response repeats.
                let front = queue
                    .front()
                    .cloned()
                    .unwrap_or_else(|| panic!("a scripted page must exist for {key}"));
                if queue.len() > 1 {
                    queue.pop_front();
                }
                front
            });
            match response {
                Some(scripted) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: scripted.headers,
                    body: scripted.body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// The real mask and key segments from a live `wasm.php` module
    /// (2026-09-25, `w=5967745`), hex-encoded.
    const LIVE_MASK: &str = "008d599d695ef560b940ba19172eecb3bf075f9cdf7ec66aab161b7e39577917";
    const LIVE_KEY: &str = "ed9281fd8b8d3b596610651a865752b4f7a214da003dfa29225f9a1886ab6bd6";

    /// The live `stream_urls` blob for Dune (`tt1160419`) that pairs
    /// with [`LIVE_MASK`]/[`LIVE_KEY`].
    const LIVE_BLOB: &str = concat!(
        "11pAThVqCYVrIung5N/Cfn7/c3ki0LL2WFEWg0c3EOH0AJESDM/IfRxHL6JDeCRv0GAdGe/ukDr5lFGBI+7gzZkobgqu87Q4d+GA",
        "9ejG71HN5hvFNByuzbOglrhLIz3vvitK+0XB2mQeSCs1I5JQhhbfHYYfIyoa8ID8YPx2posR3U2wRufGu8ORYCJG1V+HsQD1ogcD",
        "t8pCv5Vx7Ua90fMDAChiuUGJ1awZplpe2RaYmAnEYU8eP/NChy9cWvm4eiaUKnaw5pjvjyQBJu53xy776mHriv6zxH+YExZJOpTX",
        "CEMqwUnZKOc5IFwO/LJlP8CmYQoHE+1TYfLOfnu2pVenhii+eLCzJPsNpIx2j8tOsl8S4NgLsRdA5BNzXnwV6VFjsVzjzfV+JB0S",
        "qAcVH4Oid/AETmgnawDJ7TbDCYANTFrG/j8pAl0ZZLw4kdQbdm1VDBJMM4W5RusI7NBZcEzoflscNjBYhSQoFtaXOx4qmQdeilUT",
        "pkuox+7hlQzXsCuBGJDFwQTfo/9TZP2d9qgKgUEpFJUCVhgLu/WewS8TSbP+fUWyC+tPRMBSYvGxnRE0k+0ngkgMwhO9GFg9crYp",
        "H0faiMiaiNeDOG82FPH+uCbnXxOSi2bTbRAZZvAJP83Xr8R9Kaxx2HwXSScgpyiAzad4sn2wIVOm8rBVNvCWC/r9f+xsks7TGB84",
        "a00UD2I/HsuZC6fU3+q5uIP4/35u5OXqsTeiDN9+3l/AjAM2dr8UQhy3oslfTZAYP6kDkdsm5j5FYxu3rVasKlyqR+EPZ/oBQhJj",
        "NPbYMcTrcP+W/V4ZfOhvO91xq4sLnF2VCg9etZxYEpb4ZNwCXL7IiTJECrj3vQV4oXIdOOKC+fwRKKKX3pGdOik0SrdRwJK3mowr",
        "QgrsFQ0H4vuLFsVycw7BB5+0LwK4YitLZtHvzKbfX9RDomIHFmKyzR4cW6x7bRSfxGMkGBJl6MGWw7RQdDfKFv05ftggjZ2mrsu5",
        "KJAn0oUzHydGGpB19CGB4kKaXRUJ8MfAs1uCmkdCxkEYu/QC7I9xXQWf5bjayQtw6cGVadc0MekqfsD/bLgvqnbpLcIJ/e2Qojsk",
        "iqiOPcDaLYvIzzYHciqbEEqeg2FhjpCRTnI8HcbdCRH34ACTvIpVL6omdTJh2wSazb8Ue2Dcjr4Zy0BFOOlplY7k1NiIatE+CICg",
        "/IxMRWuy+n/5FufHI0I3x7zOpyyXXXosy6xeILqqZfs8fDvuPz4ncq4cblVXZ2DR7H+0XiKTFfnBTGTc9IxHQArPOJC987RRYH/1",
        "nXndHRF4vo0/ZCjuYt0HBm86aJCIfVzxaM/4jdiaCmS2D29W/v6u+BZLzM8xc1R8UHDrzpv54n38OkIhXSn+l3gdKWtIjT6PUiLX",
        "AYq7UooLWhfwIVAr"
    );

    /// The first decrypted URL of the live blob (ground truth from
    /// executing the real module).
    const LIVE_URL_1: &str = "https://loupeandlattice.site/pl/H4sIAAAAAAAAAwXBbXOCIAAA4L8ECCx3tw.rDLPJktfGNxGaXnh5q9vUX7_noXlEKMOe0o5uKAT.BVOKQGivbYC.i6_dGgZhJ.jVN9HljThNnCgOd5mmD5cS0SrtDez_uKpWe6lmkXEaDW8kew6euRNH.b27vGOdBDsBKA3c7uqsquOeN3XWgK_hgTmCswa8D_CA2zTxdhRFSH3r1uI3lO7sx37xVkCp3a0rzbOWj9mMRxwY_wysstxuSGREmiV32uo52v5HLfmOI7MoVK_apu1Zgbd_KDOwQO0AAAA-/master.m3u8";

    /// Build a minimal wasm-shaped body whose data section carries the
    /// two 32-byte segments — chars stay ≤ U+00FF so the byte
    /// reconstruction round-trips.
    fn synthetic_wasm(first: &[u8], second: &[u8]) -> String {
        let mut payload = vec![2u8]; // two segments
        for (offset, segment) in [(0u32, first), (64u32, second)] {
            payload.push(0x00); // memidx
            payload.push(0x41); // i32.const
            payload.push(u8::try_from(offset).unwrap_or(0)); // LEB offset
            payload.push(0x0b); // end
            payload.push(u8::try_from(segment.len()).unwrap_or(0)); // segment size
            payload.extend_from_slice(segment);
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\0asm");
        bytes.extend_from_slice(&[1, 0, 0, 0]);
        bytes.push(11); // data section
        bytes.push(u8::try_from(payload.len()).unwrap_or(255));
        bytes.extend_from_slice(&payload);
        bytes.iter().map(|byte| char::from(*byte)).collect()
    }

    /// Hex-decode a 32-byte fixture.
    fn hex32(hex: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (index, slot) in out.iter_mut().enumerate() {
            let head = hex
                .get(index * 2..index * 2 + 2)
                .unwrap_or_else(|| panic!("32 bytes of hex: {hex}"));
            *slot = u8::from_str_radix(head, 16)
                .unwrap_or_else(|e| panic!("valid hex byte {head}: {e}"));
        }
        out
    }

    /// Base64-encode for fixture construction (tests only).
    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).map_or(0, |b| u32::from(*b));
            let b2 = chunk.get(2).map_or(0, |b| u32::from(*b));
            let triple = b0 << 16 | b1 << 8 | b2;
            out.push(char::from(ALPHABET[(triple >> 18 & 63) as usize]));
            out.push(char::from(ALPHABET[(triple >> 12 & 63) as usize]));
            if chunk.len() > 1 {
                out.push(char::from(ALPHABET[(triple >> 6 & 63) as usize]));
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(char::from(ALPHABET[(triple & 63) as usize]));
            } else {
                out.push('=');
            }
        }
        out
    }

    /// Encrypt `plaintext` with the same cipher the module decrypts with
    /// (`ChaCha` is a XOR stream, so this is the inverse by construction).
    fn encrypt_for_test(plaintext: &str, mask: &[u8; 32], key: &[u8; 32]) -> String {
        let mut key_words = [0u32; 8];
        for (index, word) in key_words.iter_mut().enumerate() {
            *word = le32(mask, index * 4) ^ le32(key, index * 4);
        }
        // A fixed nonce stands in for the blob's prefix.
        let nonce = b"0123456789ab";
        let state = key_state(&key_words, nonce);
        let keystream = keystream_bytes(&state, plaintext.len());
        let mut blob = nonce.to_vec();
        blob.extend(plaintext.bytes().zip(keystream).map(|(byte, k)| byte ^ k));
        base64_encode(&blob)
    }

    /// A resolved Dune movie.
    fn dune_media() -> vsources_core::traits::ResolvedMedia {
        vsources_core::traits::ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: Some("tt1160419".to_string()),
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        }
    }

    /// A Dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::tmdb(438_631, MediaType::Movie)
    }

    /// A provider over the shared mock's TMDB client.
    fn provider(fetcher: &Arc<MockFetcher>) -> IMDBPlay {
        IMDBPlay::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
    }

    /// A resolve context over the shared mock.
    fn ctx_for(
        fetcher: &MockFetcher,
        media: Option<vsources_core::traits::ResolvedMedia>,
    ) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// The keystream matches RFC 8439 §2.3.2 exactly (key `00..1f`,
    /// nonce `000000090000004a00000000`, counter 1) — a regression
    /// guard for the constant table and the round structure.
    #[test]
    fn chacha_matches_the_rfc_8439_vector() {
        let key_bytes: Vec<u8> = (0u8..32).collect();
        let mut key_words = [0u32; 8];
        for (index, word) in key_words.iter_mut().enumerate() {
            *word = le32(&key_bytes, index * 4);
        }
        let nonce: [u8; 12] = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let state = key_state(&key_words, &nonce);
        let block = chacha20_block(&state, 1);
        assert_eq!(
            &block[..16],
            &[
                0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20,
                0x71, 0xc4
            ]
        );
    }

    /// The decrypted live blob reproduces the real module's output
    /// byte-for-byte.
    #[test]
    fn decrypts_the_live_ground_truth() {
        let wasm = synthetic_wasm(&hex32(LIVE_MASK), &hex32(LIVE_KEY));
        let urls = decrypt_stream_urls(LIVE_BLOB, &wasm)
            .unwrap_or_else(|| panic!("the live blob must decrypt"));
        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0], LIVE_URL_1);
        assert!(urls.iter().all(|url| url.starts_with("https://")));
    }

    #[test]
    fn decrypt_round_trips_a_synthetic_blob() {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let wasm = synthetic_wasm(mask, key);
        let plaintext =
            "https://a.example/pl/one/master.m3u8\nhttps://b.example/pl/two/master.m3u8";
        let blob = encrypt_for_test(plaintext, mask, key);

        let urls = decrypt_stream_urls(&blob, &wasm)
            .unwrap_or_else(|| panic!("the synthetic blob must decrypt"));
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://a.example/pl/one/master.m3u8");
        assert_eq!(urls[1], "https://b.example/pl/two/master.m3u8");
    }

    #[test]
    fn binary_mangled_bodies_fail_closed() {
        // A lossy-decoded binary contains chars above U+00FF.
        let mangled = "\u{0}random\u{e9}\u{1f642}wasm\u{0}junk";
        assert!(decrypt_stream_urls("AAAA", mangled).is_none());
        // Not exactly two 32-byte segments either.
        let short = [b'x'; 16];
        assert!(decrypt_stream_urls("AAAA", &synthetic_wasm(&short, &short)).is_none());
    }

    #[tokio::test]
    async fn resolves_the_full_chain_to_tokened_hls() -> Result<(), SourceError> {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let plaintext = "https://streamhost.example/pl/xyz/master.m3u8";
        let blob = encrypt_for_test(plaintext, mask, key);
        let file_name = "Dune.2021.1080p.WEB-DL.Hindi-English.DD5.1.ESub.x264-HDHub4u.Tv.mkv";
        let wasm = synthetic_wasm(mask, key);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "proxy.garageband.rocks/vs_src.php",
                    Scripted::json(&serde_json::json!({
                        "src": "https://cloudorchestranova.com/embed/movie/tt1160419?vs=abc"
                    })),
                )
                .serve(
                    "cloudorchestranova.com/embed/movie/tt1160419",
                    Scripted::text("<html></html>"),
                )
                .serve(
                    "data.vidsrcme.ru/api.php",
                    Scripted::json(&serde_json::json!({
                        "data": {"file_name": file_name, "stream_urls": blob},
                        "vs": {"w": 123, "wasm_url": "https://data.vidsrcme.ru/wasm.php?w=123"}
                    })),
                )
                .serve("data.vidsrcme.ru/wasm.php", Scripted::text(wasm))
                .serve(
                    "streamhost.example/generate.php",
                    Scripted::text("jwt-token"),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the full chain must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://streamhost.example/pl/xyz/master.m3u8?token=jwt-token"
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(
            stream.label.as_deref(),
            Some("Dune (2021) — [IMDBPlay 1080p WebDL x264 Dual Audio]")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.quality.as_deref(), Some("WebDL"));
        assert_eq!(stream.meta.codec.as_deref(), Some("x264"));
        assert_eq!(stream.meta.audio, vec!["Dual Audio".to_string()]);
        assert_eq!(
            stream.meta.languages,
            vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En]
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://cloudorchestranova.com/")
        );
        // The token mint carried its own Referer.
        assert_eq!(
            fetcher
                .sent_header("streamhost.example/generate.php", "Referer")
                .as_deref(),
            Some("https://cloudorchestranova.com/")
        );
        // The data API carried the embed origin.
        assert_eq!(
            fetcher
                .sent_header("data.vidsrcme.ru/api.php", "Referer")
                .as_deref(),
            Some("https://cloudorchestranova.com/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_flows_season_and_episode() -> Result<(), SourceError> {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let plaintext = "https://streamhost.example/pl/tv/master.m3u8";
        let blob = encrypt_for_test(plaintext, mask, key);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "proxy.garageband.rocks/vs_src.php",
                    Scripted::json(&serde_json::json!({
                        "src": "https://cloudorchestranova.com/embed/tv/tt0903747?vs=abc"
                    })),
                )
                .serve(
                    "cloudorchestranova.com/embed/tv/tt0903747",
                    Scripted::text("<html></html>"),
                )
                .serve(
                    "data.vidsrcme.ru/api.php",
                    Scripted::json(&serde_json::json!({
                        "data": {"file_name": "Breaking.Bad.S01E02.1080p.English.x264.mkv", "stream_urls": blob},
                        "vs": {"w": 123, "wasm_url": "https://data.vidsrcme.ru/wasm.php?w=123"}
                    })),
                )
                .serve(
                    "data.vidsrcme.ru/wasm.php",
                    Scripted::text(synthetic_wasm(mask, key)),
                )
                .serve(
                    "streamhost.example/generate.php",
                    Scripted::text("jwt-token"),
                ),
        );
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(vsources_core::traits::ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: Some("tt0903747".to_string()),
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(1),
                episode: Some(2),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 1, 2);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series chain must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 — [IMDBPlay 1080p WebDL x264 English]")
        );
        let vs_query = fetcher.query_of("proxy.garageband.rocks/vs_src.php");
        assert!(
            vs_query.contains("type=tv")
                && vs_query.contains("season=1")
                && vs_query.contains("episode=2"),
            "the series query must carry type/season/episode: {vs_query}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn imdb_keyed_references_resolve_through_tmdb_find() -> Result<(), SourceError> {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let blob = encrypt_for_test("https://streamhost.example/pl/dune/master.m3u8", mask, key);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "api.themoviedb.org/3/find/tt1160419",
                    Scripted::json(&serde_json::json!({"movie_results": [{"id": 438_631}]})),
                )
                .serve(
                    "api.themoviedb.org/3/movie/438631",
                    Scripted::json(&serde_json::json!({"title": "Dune", "release_date": "2021-10-22"})),
                )
                .serve(
                    "proxy.garageband.rocks/vs_src.php",
                    Scripted::json(&serde_json::json!({
                        "src": "https://cloudorchestranova.com/embed/movie/tt1160419?vs=abc"
                    })),
                )
                .serve(
                    "cloudorchestranova.com/embed/movie/tt1160419",
                    Scripted::text("<html></html>"),
                )
                .serve(
                    "data.vidsrcme.ru/api.php",
                    Scripted::json(&serde_json::json!({
                        "data": {"file_name": "Dune.2021.1080p.English.x264.mkv", "stream_urls": blob},
                        "vs": {"w": 123, "wasm_url": "https://data.vidsrcme.ru/wasm.php?w=123"}
                    })),
                )
                .serve(
                    "data.vidsrcme.ru/wasm.php",
                    Scripted::text(synthetic_wasm(mask, key)),
                )
                .serve(
                    "streamhost.example/generate.php",
                    Scripted::text("jwt-token"),
                ),
        );
        let ctx = ctx_for(&fetcher, None);
        let media = MediaRef::imdb("tt1160419", MediaType::Movie);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the IMDb-keyed chain must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://streamhost.example/pl/dune/master.m3u8?token=jwt-token"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_embed_src_is_a_miss() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "proxy.garageband.rocks/vs_src.php",
            Scripted::json(&serde_json::json!({})),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing embed src must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_text_mangled_wasm_fails_closed() {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let blob = encrypt_for_test("https://streamhost.example/pl/x/master.m3u8", mask, key);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "proxy.garageband.rocks/vs_src.php",
                    Scripted::json(&serde_json::json!({
                        "src": "https://cloudorchestranova.com/embed/movie/tt1160419?vs=abc"
                    })),
                )
                .serve(
                    "cloudorchestranova.com/embed/movie/tt1160419",
                    Scripted::text("<html></html>"),
                )
                .serve(
                    "data.vidsrcme.ru/api.php",
                    Scripted::json(&serde_json::json!({
                        "data": {"file_name": "Dune.2021.1080p.English.x264.mkv", "stream_urls": blob},
                        "vs": {"w": 123, "wasm_url": "https://data.vidsrcme.ru/wasm.php?w=123"}
                    })),
                )
                // A lossy-decoded binary body: chars beyond U+00FF.
                .serve("data.vidsrcme.ru/wasm.php", Scripted::text("mangledé🙂wasm")),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a mangled wasm must fail closed as NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn html_token_bodies_drop_their_streams() {
        let mask = b"0123456789abcdef0123456789abcdef";
        let key = b"fedcba9876543210fedcba9876543210";
        let blob = encrypt_for_test("https://streamhost.example/pl/x/master.m3u8", mask, key);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "proxy.garageband.rocks/vs_src.php",
                    Scripted::json(&serde_json::json!({
                        "src": "https://cloudorchestranova.com/embed/movie/tt1160419?vs=abc"
                    })),
                )
                .serve(
                    "cloudorchestranova.com/embed/movie/tt1160419",
                    Scripted::text("<html></html>"),
                )
                .serve(
                    "data.vidsrcme.ru/api.php",
                    Scripted::json(&serde_json::json!({
                        "data": {"file_name": "Dune.2021.1080p.English.x264.mkv", "stream_urls": blob},
                        "vs": {"w": 123, "wasm_url": "https://data.vidsrcme.ru/wasm.php?w=123"}
                    })),
                )
                .serve(
                    "data.vidsrcme.ru/wasm.php",
                    Scripted::text(synthetic_wasm(mask, key)),
                )
                .serve(
                    "streamhost.example/generate.php",
                    Scripted::text("<html>denied</html>"),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("an HTML token body must drop the stream, got {other:?}"),
        }
    }

    #[test]
    fn label_parsing_covers_the_upstream_mappings() {
        assert_eq!(parse_height("2160p"), 2160);
        assert_eq!(parse_height("4k"), 2160);
        assert_eq!(parse_height("1080p"), 1080);
        assert_eq!(parse_height("720p"), 720);
        assert_eq!(parse_height("480p"), 480);
        assert_eq!(parse_height("plain"), 1080);
        assert_eq!(parse_codec("x265"), "HEVC");
        assert_eq!(parse_codec("h.264"), "x264");
        assert_eq!(parse_source_type("remux"), "BluRay Remux");
        assert_eq!(parse_source_type("bdrip"), "BluRay");
        assert_eq!(parse_source_type("web-dl"), "WebDL");
        assert_eq!(parse_source_type("webrip"), "WebRip");
        assert_eq!(parse_source_type("hdrip"), "HDRip");
        assert_eq!(parse_language("hindi.english"), "Dual Audio");
        assert_eq!(parse_language("hindi"), "Hindi");
        assert_eq!(parse_language("english"), "English");
        assert_eq!(parse_language("nothing"), "English");
    }
}
