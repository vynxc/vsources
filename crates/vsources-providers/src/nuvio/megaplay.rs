//! Megaplay `getSources` decryption — the port of
//! `src/nuvio/megaplay_decrypt.cjs`.
//!
//! 2026-09: `megaplay.buzz`'s sources API stopped returning plaintext
//! `{ sources: { file } }`; it now answers
//! `{ tracks: […], t, intro, outro, server, enc: "<urlsafe-base64>" }`
//! where `enc` decrypts (AES-256-CBC) to `{ file: "<master.m3u8>" }`.
//! The key/IV ship inside the site's player bundle
//! (`newclient.min.js`):
//!
//! - key: the 16-char `i?LMTAx0Q6,:}50U` zero-padded to 32 bytes;
//! - IV: `W0;27ToaUpl_P%'c` as-is.
//!
//! The plaintext subtitle `tracks` ride along unchanged.
//!
//! Cut from the JS: `installMegaplayShim` — it patched Node's global
//! `fetch` once so every obfuscated scraper kept seeing the legacy
//! shape. There is no global fetch to patch here (and the Rust Megaplay
//! extractor decrypts `enc` directly), so only
//! [`decrypt_megaplay_enc`] remains.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use super::decrypt::aes256_cbc_decrypt;

/// `W("i?LMTAx0Q6,:}50U", 32)` — the raw key from the player bundle.
const MP_AES_KEY_RAW: &str = "i?LMTAx0Q6,:}50U";
/// `W("W0;27ToaUpl_P%'c", 16)` — the raw IV.
const MP_AES_IV_RAW: &str = "W0;27ToaUpl_P%'c";

/// The decrypted payload shape — `{ file: "https://…master.m3u8" }`.
#[derive(Debug, Deserialize)]
struct MegaplayPayload {
    /// The direct stream URL.
    file: String,
}

/// The zero-padded 32-byte AES key.
fn megaplay_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    let raw = MP_AES_KEY_RAW.as_bytes();
    key[..raw.len()].copy_from_slice(raw);
    key
}

/// The raw 16-byte IV.
fn megaplay_iv() -> [u8; 16] {
    let mut iv = [0u8; 16];
    let raw = MP_AES_IV_RAW.as_bytes();
    iv[..raw.len()].copy_from_slice(raw);
    iv
}

/// Decrypt the `enc` blob → the stream file URL — ports
/// `decryptMegaplayEnc`. `None` on any decode/decrypt/parse failure,
/// matching the JS catch-and-return-null.
#[must_use]
pub fn decrypt_megaplay_enc(enc: &str) -> Option<String> {
    let ciphertext = URL_SAFE_NO_PAD.decode(enc).ok()?;
    let plaintext = aes256_cbc_decrypt(&megaplay_key(), &megaplay_iv(), &ciphertext)?;
    let payload: MegaplayPayload = serde_json::from_slice(&plaintext).ok()?;
    if payload.file.is_empty() {
        None
    } else {
        Some(payload.file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth generated with Node's `crypto` using the exact
    /// upstream key/IV: encrypting
    /// `{"file":"https://megap.akirax.buzz/hls/abc123/master.m3u8?token=xyz"}`
    /// yields this urlsafe-base64 `enc` blob.
    const GROUND_TRUTH_ENC: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfX7M-F6BNk8LV386Hu9tS4mFUNr-s_AJy1VBvh7NDicJy9Oxe9Oq___572UgI-DEIA";

    #[test]
    fn decrypts_the_ground_truth_blob() {
        assert_eq!(
            decrypt_megaplay_enc(GROUND_TRUTH_ENC).as_deref(),
            Some("https://megap.akirax.buzz/hls/abc123/master.m3u8?token=xyz")
        );
    }

    #[test]
    fn fails_closed_on_garbage() {
        assert!(decrypt_megaplay_enc("").is_none());
        assert!(decrypt_megaplay_enc("!!!not-base64!!!").is_none());
        // Valid base64 of a valid ciphertext length but wrong content.
        assert!(decrypt_megplay_wrong_blob().is_none());
    }

    /// A blob that decrypts (padding removed) to non-JSON.
    fn decrypt_megplay_wrong_blob() -> Option<String> {
        // "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" decodes to 24 zero bytes —
        // a multiple of the block size, but the padding check fails.
        decrypt_megaplay_enc("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
    }
}
