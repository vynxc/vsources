//! The stream-decryption toolkit — the port of
//! `src/utils/stream-decrypt.cjs` (§5/§6/§9.4 of the upstream
//! source-onboarding guide), plus the `FlixCloud` key chain it shares
//! with `reanime.cjs`.
//!
//! Pure, dependency-light helpers so any future encrypted source can be
//! onboarded without re-implementing them:
//!
//! - [`xor_decrypt`] — cyclic XOR.
//! - [`base64_decode_strict`] — plausibility-checked base64.
//! - [`is_webp_disguise`] / [`is_png_disguise`] /
//!   [`strip_fake_image_header`] — §5.2 image-disguised segments.
//! - [`verify_mpeg_ts`] — §9.4 sync-byte verification.
//! - [`derive_aes_key_chain`] — §5.4 PBKDF2 → XOR-seed → SHA-256 key
//!   derivation (hand-implemented HMAC-SHA256: the workspace ships no
//!   `pbkdf2`/`hmac` crate).
//! - [`aes256_cbc_decrypt`] — §5.4 AES-256-CBC.
//! - [`sha256_hex`] / [`derive_field_names`] — §5.5 obfuscated field
//!   names (the `FlixCloud` variant, with all seven fields).
//! - [`detect_and_decrypt`] — §6.4 content-type auto-dispatch.
//! - [`parse_xor_key_param`] — proxy-style key parsing.
//!
//! Cut from the JS: `decryptProxyResponse` (the Express handler — no
//! server here) and `createWasmRunner` (§5.3) — WebAssembly execution
//! has no crate in the workspace; the `FlixCloud` WASM is *parsed*
//! instead (see [`crate::nuvio::flixcloud`]).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

/// The raw `aes` block size in bytes.
const AES_BLOCK: usize = 16;

/// HMAC-SHA256 (FIPS 198-1) — hand-rolled because the workspace has no
/// `hmac` crate; used only by [`derive_aes_key_chain`].
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

/// PBKDF2-HMAC-SHA256 (RFC 8018) — the key-len truncation matches
/// `crypto.pbkdf2Sync`.
fn pbkdf2_sha256(secret: &[u8], salt: &[u8], iterations: u32, key_len: usize) -> Vec<u8> {
    let mut derived: Vec<u8> = Vec::with_capacity(key_len);
    let mut block_index: u32 = 1;
    while derived.len() < key_len {
        let mut message = salt.to_vec();
        message.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256(secret, &message).to_vec();
        let mut t = u.clone();
        for _ in 1..iterations {
            u = hmac_sha256(secret, &u).to_vec();
            for (ti, ui) in t.iter_mut().zip(u.iter()) {
                *ti ^= ui;
            }
        }
        derived.extend_from_slice(&t);
        block_index += 1;
    }
    derived.truncate(key_len);
    derived
}

/// Cyclic XOR — ports `xorDecrypt` (`Buffer.alloc` + loop).
#[must_use]
pub fn xor_decrypt(buf: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return buf.to_vec();
    }
    buf.iter()
        .zip(key.iter().cycle())
        .map(|(byte, key_byte)| byte ^ key_byte)
        .collect()
}

/// Decode only if the string is *plausibly* base64 — ports
/// `base64DecodeStrict`: no whitespace, valid alphabet, correct
/// padding, and a round-trip-length check (Node's decoder is lenient).
#[must_use]
pub fn base64_decode_strict(text: &str) -> Option<Vec<u8>> {
    let flat: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if flat.len() < 8 || !flat.len().is_multiple_of(4) {
        return None;
    }
    if !flat
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    {
        return None;
    }
    let padding = 2 * usize::from(flat.ends_with("=="))
        + usize::from(!flat.ends_with("==") && flat.ends_with('='));
    let expected = flat.len() / 4 * 3 - padding;
    let decoded = STANDARD.decode(&flat).ok()?;
    if decoded.len() == expected {
        Some(decoded)
    } else {
        None
    }
}

/// WebP-disguised body: `RIFF` + 4 length bytes + `WEBP` (12-byte fake
/// header) — ports `isWebPDisguise`.
#[must_use]
pub fn is_webp_disguise(buf: &[u8]) -> bool {
    buf.len() >= 12
        && buf[0] == 0x52
        && buf[1] == 0x49
        && buf[2] == 0x46
        && buf[3] == 0x46
        && buf[8] == 0x57
        && buf[9] == 0x45
        && buf[10] == 0x42
        && buf[11] == 0x50
}

/// PNG-disguised body: the 8-byte `89 50 4E 47 0D 0A 1A 0A` signature —
/// ports `isPngDisguise`.
#[must_use]
pub fn is_png_disguise(buf: &[u8]) -> bool {
    buf.len() >= 8
        && buf[0] == 0x89
        && buf[1] == 0x50
        && buf[2] == 0x4E
        && buf[3] == 0x47
        && buf[4] == 0x0D
        && buf[5] == 0x0A
        && buf[6] == 0x1A
        && buf[7] == 0x0A
}

/// The fake image header length: 12 (WebP), 8 (PNG), 0 (none) — ports
/// `fakeImageHeaderLen`.
#[must_use]
pub fn fake_image_header_len(buf: &[u8]) -> usize {
    if is_webp_disguise(buf) {
        12
    } else if is_png_disguise(buf) {
        8
    } else {
        0
    }
}

/// Strip the fake image header — ports the proxy's `body.slice(12)` /
/// `body.slice(8)` branches.
#[must_use]
pub fn strip_fake_image_header(buf: &[u8]) -> Vec<u8> {
    let skip = fake_image_header_len(buf);
    if skip > 0 && buf.len() > skip {
        buf[skip..].to_vec()
    } else if skip > 0 {
        Vec::new()
    } else {
        buf.to_vec()
    }
}

/// MPEG-TS verification result — ports `verifyMpegTs`'s return shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsVerification {
    /// Whether every checked packet carries the 0x47 sync byte.
    pub valid: bool,
    /// How many packets were checked (≤ `max_packets`).
    pub checked: usize,
    /// Total packets in the buffer.
    pub packets: usize,
}

/// Verify decrypted bytes are valid MPEG-TS: the 0x47 sync byte every
/// 188 bytes, up to `max_packets` (8) — ports `verifyMpegTs`.
#[must_use]
pub fn verify_mpeg_ts(buf: &[u8], max_packets: usize) -> TsVerification {
    const PACKET: usize = 188;
    if buf.len() < PACKET {
        return TsVerification {
            valid: false,
            checked: 0,
            packets: 0,
        };
    }
    let total = buf.len() / PACKET;
    let checked = max_packets.min(total);
    for packet in 0..checked {
        if buf[packet * PACKET] != 0x47 {
            return TsVerification {
                valid: false,
                checked: packet + 1,
                packets: total,
            };
        }
    }
    TsVerification {
        valid: true,
        checked,
        packets: total,
    }
}

/// Derive the 32-byte AES key from a secret and per-request seed —
/// ports `deriveAesKeyChain` (and the identical chain in
/// `reanime.cjs`): PBKDF2(secret, seed) → XOR with the seed bytes →
/// SHA-256.
#[must_use]
pub fn derive_aes_key_chain(secret: &[u8], seed: &str, iterations: u32) -> [u8; 32] {
    let w = pbkdf2_sha256(secret, seed.as_bytes(), iterations, 32);
    let seed_bytes = seed.as_bytes();
    let tt: Vec<u8> = w
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ seed_bytes[i % seed_bytes.len()])
        .collect();
    Sha256::digest(&tt).into()
}

/// AES-256-CBC decrypt (PKCS#7) → bytes; `None` on bad key/IV/length —
/// the JS decipher's thrown error.
#[must_use]
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 32
        || iv.len() != AES_BLOCK
        || data.is_empty()
        || !data.len().is_multiple_of(AES_BLOCK)
    {
        return None;
    }
    let decryptor = cbc::Decryptor::<aes::Aes256>::new(
        GenericArray::from_slice(key),
        GenericArray::from_slice(iv),
    );
    let mut buffer = data.to_vec();
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?
        .to_vec();
    Some(plaintext)
}

/// SHA-256 of a UTF-8 string, hex-encoded — ports `sha256Hex`.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The per-request obfuscated field names — ports the `FlixCloud`
/// `deriveFieldNames` (the `xn()` of `12.ynMYRcYB.js`): the seed is
/// chained 3× through SHA-256 for `hash_e`, 3× more for `hash_a`, and
/// the fields slice into both hex strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldNames {
    /// `cd_` + `hash_e[24..32]` — the crypto container object.
    pub container_name: String,
    /// `ad_` + `hash_e[32..40]` — the array inside it.
    pub array_name: String,
    /// `od_` + `hash_e[40..48]` — the object inside that.
    pub object_name: String,
    /// `kf_` + `hash_e[8..16]` — the first key fragment field.
    pub key_field: String,
    /// `ivf_` + `hash_e[16..24]` — the IV field.
    pub iv_field: String,
    /// `hash_e[48..64]` + `_` + `hash_e[56..64]` — the token field.
    pub token_field: String,
    /// `hash_a[0..16]` + `_` + `hash_a[16..24]` — the second key
    /// fragment field.
    pub key_frag2_field: String,
}

/// Derive the `FlixCloud` field names from the page's
/// `obfuscation_seed` — see [`FieldNames`].
#[must_use]
pub fn derive_field_names(seed: &str) -> FieldNames {
    let mut e = seed.to_string();
    for i in 0..3 {
        e = sha256_hex(&format!("{e}{i}"));
    }
    let mut a = e.clone();
    for i in 0..3 {
        a = sha256_hex(&format!("{a}{i}"));
    }
    let sub_e =
        |start: usize, end: usize| -> String { e.chars().skip(start).take(end - start).collect() };
    let sub_a =
        |start: usize, end: usize| -> String { a.chars().skip(start).take(end - start).collect() };
    FieldNames {
        container_name: format!("cd_{}", sub_e(24, 32)),
        array_name: format!("ad_{}", sub_e(32, 40)),
        object_name: format!("od_{}", sub_e(40, 48)),
        key_field: format!("kf_{}", sub_e(8, 16)),
        iv_field: format!("ivf_{}", sub_e(16, 24)),
        token_field: format!("{}_{}", sub_e(48, 64), sub_e(56, 64)),
        key_frag2_field: format!("{}_{}", sub_a(0, 16), sub_a(16, 24)),
    }
}

/// What [`detect_and_decrypt`] decided the body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecryptedKind {
    /// An MPEG-TS segment (or a stripped/XOR'd body with a key).
    Segment,
    /// An HLS playlist (`#EXTM3U` text).
    Playlist,
    /// No key and no recognizable shape — pass through.
    Passthrough,
}

/// The outcome of [`detect_and_decrypt`] — ports the JS return shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedBody {
    /// The detected kind.
    pub kind: DecryptedKind,
    /// The decrypted payload bytes.
    pub payload: Vec<u8>,
    /// The content type to serve the payload as.
    pub content_type: &'static str,
    /// The playlist text, when `kind` is
    /// [`DecryptedKind::Playlist`].
    pub playlist_text: Option<String>,
}

/// Options for [`detect_and_decrypt`] — the XOR key, a custom fake
/// header length, and the caller-asserted base64 flag.
#[derive(Debug, Clone, Default)]
pub struct DecryptOptions<'a> {
    /// The cyclic XOR key, when the body is encrypted.
    pub xor_key: Option<&'a [u8]>,
    /// A site-declared fake header length (overrides magic bytes).
    pub strip_hint: usize,
    /// The caller asserts the body is base64 (guide §6.2).
    pub expect_base64: bool,
}

/// Detect what an upstream body is and decrypt it accordingly — ports
/// `detectAndDecrypt` (§6.4): fake-image disguises first, then plain
/// playlists, then asserted/auto base64, then whole-body XOR, else
/// passthrough.
#[must_use]
pub fn detect_and_decrypt(body: &[u8], options: &DecryptOptions<'_>) -> DecryptedBody {
    let xor_key = options.xor_key.unwrap_or(&[]);
    let is_m3u8 = |bytes: &[u8]| bytes.starts_with(b"#EXTM3U");
    let playlist = |plain: &[u8]| DecryptedBody {
        kind: DecryptedKind::Playlist,
        payload: plain.to_vec(),
        content_type: "application/vnd.apple.mpegurl",
        playlist_text: Some(String::from_utf8_lossy(plain).into_owned()),
    };
    let segment = |plain: Vec<u8>| DecryptedBody {
        kind: DecryptedKind::Segment,
        payload: plain,
        content_type: "video/mp2t",
        playlist_text: None,
    };

    // 1-2. Fake-image-disguised segments.
    let disguise = fake_image_header_len(body);
    if disguise > 0 && !xor_key.is_empty() {
        return segment(xor_decrypt(&body[disguise..], xor_key));
    }

    // 3. A site-declared header length.
    if options.strip_hint > 0 && options.strip_hint < body.len() && !xor_key.is_empty() {
        return segment(xor_decrypt(&body[options.strip_hint..], xor_key));
    }

    // 4. A plain playlist.
    if is_m3u8(body) {
        return playlist(body);
    }

    // Steps 5-6 (asserted + auto base64): the JS runs two identical
    // checks — the asserted one falls through to the auto one, so a
    // single pass covers both. Decode + optional XOR must yield an
    // #EXTM3U playlist.
    let text = String::from_utf8_lossy(body);
    let _ = options.expect_base64;
    if let Some(decoded) = base64_decode_strict(&text) {
        let plain = if xor_key.is_empty() {
            decoded
        } else {
            xor_decrypt(&decoded, xor_key)
        };
        if is_m3u8(&plain) {
            return playlist(&plain);
        }
    }

    // 7. Whole-body XOR — playlist-shaped wins, else a TS segment.
    if !xor_key.is_empty() {
        let plain = xor_decrypt(body, xor_key);
        if is_m3u8(&plain) {
            return playlist(&plain);
        }
        return segment(plain);
    }

    // No key — pass through.
    DecryptedBody {
        kind: DecryptedKind::Passthrough,
        payload: body.to_vec(),
        content_type: "application/octet-stream",
        playlist_text: None,
    }
}

/// Parse a proxy-style `xor` key parameter into bytes — ports
/// `parseXorKeyParam`: an even-length pure-hex string (≥ 8 chars) is
/// hex; anything else that decodes to ≥ 4 bytes is base64.
#[must_use]
pub fn parse_xor_key_param(raw: &str) -> Option<Vec<u8>> {
    if raw.is_empty() {
        return None;
    }
    let is_hex =
        raw.len() >= 8 && raw.len().is_multiple_of(2) && raw.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex {
        let mut bytes = Vec::with_capacity(raw.len() / 2);
        for pair in raw.as_bytes().chunks(2) {
            let hex = std::str::from_utf8(pair).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
        }
        return Some(bytes);
    }
    let decoded = STANDARD.decode(raw).ok()?;
    if decoded.len() >= 4 {
        Some(decoded)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    #[test]
    fn pbkdf2_sha256_matches_known_vector() {
        // PBKDF2-HMAC-SHA256("passwd", "salt", 1, 32) — the classic
        // public test vector.
        let derived = pbkdf2_sha256(b"passwd", b"salt", 1, 32);
        assert_eq!(
            derived,
            [
                0x55, 0xac, 0x04, 0x6e, 0x56, 0xe3, 0x08, 0x9f, 0xec, 0x16, 0x91, 0xc2, 0x25, 0x44,
                0xb6, 0x05, 0xf9, 0x41, 0x85, 0x21, 0x6d, 0xde, 0x04, 0x65, 0xe6, 0x8b, 0x9d, 0x57,
                0xc2, 0x0d, 0xac, 0xbc
            ]
        );
    }

    #[test]
    fn derive_aes_key_chain_is_deterministic() {
        // The O fragment from the live flixcloud capture (see
        // flixcloud.rs for the end-to-end ground truth).
        let o = [
            0x36, 0x1f, 0x49, 0x6a, 0xd6, 0x9b, 0xa3, 0x58, 0x5b, 0x70, 0xcd, 0x06, 0xf2, 0xed,
            0xcf, 0x1b, 0xc3, 0xab, 0x8d, 0x8a, 0x37, 0xa7, 0x20, 0xc8, 0x1e, 0xe9, 0xef, 0xc7,
            0x02, 0xf4, 0xc0, 0x8d,
        ];
        let key = derive_aes_key_chain(&o, "d231526af61cb187", 1000);
        assert_eq!(key.len(), 32);
        assert_eq!(key, derive_aes_key_chain(&o, "d231526af61cb187", 1000));
        assert_ne!(key, derive_aes_key_chain(&o, "another-seed", 1000));
    }

    #[test]
    fn xors_cyclically() {
        assert_eq!(xor_decrypt(b"ab", &[0x01]), vec![b'`', b'c']);
        assert_eq!(
            xor_decrypt(&xor_decrypt(b"payload", b"key"), b"key"),
            b"payload"
        );
        // An empty key is the identity (the JS guard).
        assert_eq!(xor_decrypt(b"abc", &[]), b"abc");
    }

    #[test]
    fn decodes_strict_base64() {
        let encoded = STANDARD.encode(b"#EXTM3U\nplain playlist text");
        assert_eq!(
            base64_decode_strict(&encoded).as_deref(),
            Some(b"#EXTM3U\nplain playlist text".as_slice())
        );
        // Whitespace breaks the strict form.
        assert!(base64_decode_strict("abc def+").is_none());
        // Non-alphabet characters break it.
        assert!(base64_decode_strict("ab!cd!ef").is_none());
        // Too short.
        assert!(base64_decode_strict("abc").is_none());
        assert!(base64_decode_strict("").is_none());
        // Padding arithmetic: one and two padding chars round-trip.
        let one_pad = STANDARD.encode(b"1234567");
        let two_pad = STANDARD.encode(b"123456");
        assert_eq!(
            base64_decode_strict(&one_pad).as_deref(),
            Some(b"1234567".as_slice())
        );
        assert_eq!(
            base64_decode_strict(&two_pad).as_deref(),
            Some(b"123456".as_slice())
        );
    }

    #[test]
    fn detects_image_disguises() {
        let webp = [
            0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50, 1, 2, 3,
        ];
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2];
        assert!(is_webp_disguise(&webp));
        assert_eq!(fake_image_header_len(&webp), 12);
        assert!(is_png_disguise(&png));
        assert_eq!(fake_image_header_len(&png), 8);
        assert_eq!(fake_image_header_len(b"#EXTM3U"), 0);
        assert_eq!(strip_fake_image_header(&webp), vec![1, 2, 3]);
        assert_eq!(strip_fake_image_header(&png), vec![1, 2]);
        assert_eq!(strip_fake_image_header(b"#EXTM3U"), b"#EXTM3U".to_vec());
    }

    #[test]
    fn verifies_mpeg_ts_sync_bytes() {
        let mut ts = vec![0x47; 188 * 4];
        ts.push(0x47);
        let check = verify_mpeg_ts(&ts, 8);
        assert!(check.valid);
        assert_eq!(check.checked, 4);
        assert_eq!(check.packets, 4);
        ts[188] = 0x00;
        let broken = verify_mpeg_ts(&ts, 8);
        assert!(!broken.valid);
        assert_eq!(broken.checked, 2);
        let empty = verify_mpeg_ts(&ts[..100], 8);
        assert!(!empty.valid);
    }

    #[test]
    fn derives_flixcloud_field_names_from_live_seed() {
        // Captured from the live flixcloud.cc /e/ page with seed
        // d231526af61cb187 (node crypto reproduces the JS values).
        let fields = derive_field_names("d231526af61cb187");
        assert_eq!(fields.container_name, "cd_2e15a574");
        assert_eq!(fields.array_name, "ad_5e624ac7");
        assert_eq!(fields.object_name, "od_e28700f6");
        assert_eq!(fields.key_field, "kf_da67e9be");
        assert_eq!(fields.iv_field, "ivf_cb9d11d5");
        assert_eq!(fields.token_field, "42446185cbe125a1_cbe125a1");
        assert_eq!(fields.key_frag2_field, "3893eee5f98fb215_d6b7d80f");
    }

    #[test]
    fn decrypts_and_detects_each_body_kind() {
        // A WebP-disguised, XOR-encrypted TS segment.
        let mut segment: Vec<u8> = vec![0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50];
        segment.extend(std::iter::repeat_n(0x47u8, 188));
        let key = [0x00u8, 0xff];
        let result = detect_and_decrypt(
            &segment,
            &DecryptOptions {
                xor_key: Some(&key),
                ..DecryptOptions::default()
            },
        );
        assert_eq!(result.kind, DecryptedKind::Segment);
        assert_eq!(result.content_type, "video/mp2t");
        assert!(verify_mpeg_ts(&result.payload, 4).valid);

        // A plain playlist passes through as a playlist.
        let playlist = b"#EXTM3U\n#EXT-X-VERSION:3\n";
        let result = detect_and_decrypt(playlist, &DecryptOptions::default());
        assert_eq!(result.kind, DecryptedKind::Playlist);
        assert_eq!(
            result.playlist_text.as_deref(),
            Some("#EXTM3U\n#EXT-X-VERSION:3\n")
        );

        // A base64+XOR playlist (FlixCloud's variant shape).
        let plain = b"#EXTM3U\nvariant";
        let xor_key = [0x0fu8, 0x1e];
        let xored = xor_decrypt(plain, &xor_key);
        let encoded = STANDARD.encode(&xored);
        let result = detect_and_decrypt(
            encoded.as_bytes(),
            &DecryptOptions {
                xor_key: Some(&xor_key),
                ..DecryptOptions::default()
            },
        );
        assert_eq!(result.kind, DecryptedKind::Playlist);
        assert_eq!(result.payload, plain.to_vec());

        // A whole-body-XOR segment with no playlist shape.
        let body = vec![0x11u8, 0x22, 0x33];
        let result = detect_and_decrypt(
            &body,
            &DecryptOptions {
                xor_key: Some(&xor_key),
                ..DecryptOptions::default()
            },
        );
        assert_eq!(result.kind, DecryptedKind::Segment);

        // No key, no shape → passthrough.
        let result = detect_and_decrypt(b"\x01\x02\x03", &DecryptOptions::default());
        assert_eq!(result.kind, DecryptedKind::Passthrough);
        assert_eq!(result.content_type, "application/octet-stream");
    }

    #[test]
    fn parses_xor_key_params() {
        assert_eq!(
            parse_xor_key_param("deadbeef"),
            Some(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(
            parse_xor_key_param(&STANDARD.encode(b"abcd")),
            Some(b"abcd".to_vec())
        );
        assert!(parse_xor_key_param("").is_none());
        assert!(parse_xor_key_param("ab").is_none());
    }
}
