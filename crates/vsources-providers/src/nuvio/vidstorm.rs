//! vidstorm.ru stream-token decryption — the port of
//! `src/utils/vidstorm-decrypt.cjs`.
//!
//! `vidstorm.ru/api/movie/{tmdbId}` and `/api/tv/{tmdbId}/{s}/{e}`
//! answer `{ lithium: {url,…}, helium: {…}, carbon: {…}, … }` where
//! `url` is a base64url AES-256-GCM blob `[12-byte IV][ciphertext +
//! 16-byte tag]` decrypting to the real stream URL. The key derivation
//! is verbatim from the SPA bundle:
//!
//! ```text
//! gQ = "C1oPWQVf…" ; key = b64decode(gQ) → each byte XOR 60
//!     → ASCII hex string (64 chars) → parse as hex → 32 raw bytes
//! ```
//!
//! Playback needs `Origin: https://vidstorm.ru` on every request; the
//! provider layer attaches that (upstream routed it through its proxy).
//!
//! The AES-256-GCM decryption is the same minimal SP 800-38D
//! construction the Vidzee extractor uses — the workspace ships no GCM
//! crate.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fancy_regex::Regex;
use std::sync::LazyLock;

/// The bundle's base64 key constant (`gQ`).
const GQ: &str =
    "C1oPWQVfDl0EXglYDVoIWQpdBV8PXgtYDlkJWgRdDV8IXgpYBVkOWgldBF8NXghYC1kFWg5dCV8EXg1YCFkLWg==";
/// The per-byte XOR applied to the decoded key constant.
const XOR_BYTE: u8 = 60;

/// Tokens that are already URLs pass through (ports the site's `oL()`
/// guard).
static ALREADY_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(https?:|blob:|data:|//)").unwrap_or_else(|e| panic!("valid URL guard: {e}"))
});

/// The derived 32-byte AES key — ports `getKey()` (computed once, like
/// the JS cache).
#[must_use]
pub fn vidstorm_key() -> [u8; 32] {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(GQ)
        .unwrap_or_else(|e| panic!("the bundle key is valid base64: {e}"));
    // Each byte XOR 60 becomes an ASCII hex character; the 64-char
    // string parses back to 32 bytes.
    let hex: String = raw.iter().map(|byte| char::from(byte ^ XOR_BYTE)).collect();
    let mut key = [0u8; 32];
    for (index, pair) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).unwrap_or("00");
        key[index] = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    key
}

/// Decrypt a vidstorm token → the real URL — ports `vidstormDecrypt`.
///
/// Already-URL tokens pass through; bodies shorter than the site's
/// `iv + tag` minimum (29 bytes) and auth failures return `None`.
#[must_use]
pub fn vidstorm_decrypt(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    if ALREADY_URL.is_match(token).unwrap_or(false) {
        return Some(token.to_string());
    }
    let buf = URL_SAFE_NO_PAD.decode(token).ok()?;
    if buf.len() < 29 {
        return None;
    }
    let (iv, rest) = buf.split_at(12);
    if rest.len() <= 16 {
        return None;
    }
    let (ciphertext, tag) = rest.split_at(rest.len() - 16);
    let key = vidstorm_key();
    let plaintext = gcm::decrypt(&key, iv, tag, ciphertext)?;
    String::from_utf8(plaintext).ok()
}

/// AES-256-GCM decryption over the raw `aes` block cipher — the same
/// minimal construction as the Vidzee extractor (empty associated
/// data, 96-bit IV).
mod gcm {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::{Aes256, Block};

    /// The GHASH reduction constant `R = E1 ‖ 0^120`.
    const R: u128 = 0xE1 << 120;

    /// Decrypt `ciphertext` under `key`, verifying `auth_tag`.
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
    use super::*;

    /// The derived key from the bundle constant, reproduced with Node's
    /// `crypto` byte-for-byte.
    #[test]
    fn derives_the_bundle_key() {
        assert_eq!(
            vidstorm_key(),
            [
                0x7f, 0x3e, 0x9c, 0x2a, 0x8b, 0x5d, 0x1f, 0x4e, 0x6a, 0x9c, 0x3b, 0x7d, 0x2e, 0x5f,
                0x8a, 0x1c, 0x4b, 0x6d, 0x9e, 0x2f, 0x5a, 0x8c, 0x1b, 0x4d, 0x7e, 0x9f, 0x2a, 0x5c,
                0x8b, 0x1d, 0x4e, 0x7f
            ]
        );
    }

    /// Ground truth: encrypting a URL with the derived key, IV
    /// `00112233445566778899aabb`, produces this token (Node's
    /// `aes-256-gcm`).
    #[test]
    fn decrypts_the_ground_truth_token() {
        let token = "ABEiM0RVZneImaq7FDvPJlEEEmCLEe0DVeKWS9pzGmAf_JJjapyyo5CCaIb3Or4zdOAh-Kmud81fyw6r4HmicL1ewPjCMCoFLgT2GiNw9dobJRMhxo6OqPP47jopNKwbfQ";
        assert_eq!(
            vidstorm_decrypt(token).as_deref(),
            Some("https://dreadnought.example.workers.dev/_v7/abc/master.m3u8?token=jwt")
        );
    }

    #[test]
    fn passes_through_urls_and_fails_closed() {
        assert_eq!(
            vidstorm_decrypt("https://direct.example/x.m3u8").as_deref(),
            Some("https://direct.example/x.m3u8")
        );
        assert_eq!(
            vidstorm_decrypt("https://a.example/x.m3u8").as_deref(),
            Some("https://a.example/x.m3u8")
        );
        assert!(vidstorm_decrypt("").is_none());
        assert!(vidstorm_decrypt("AAAA").is_none());
        // Long enough to pass the guard but not valid GCM.
        assert!(vidstorm_decrypt(&"A".repeat(48)).is_none());
    }
}
