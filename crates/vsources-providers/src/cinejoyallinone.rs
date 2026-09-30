//! `CineJoyAllInOne`: the `cinejoy.pk` sealed-API aggregator.
//!
//! Ports `src/source/CineJoyAllInOne.js` plus the single-file scraper it
//! loads, `src/nuvio/cinejoy_all_in_one.cjs` — the Nuvio source that
//! speaks the site's `lumen-gate-v2` handshake against
//! `api.wing.st`. The flow:
//!
//! 1. **TMDB** — id (mapping `IMDb` references through
//!    [`TmdbClient::tmdb_id_from_imdb`]) plus name/year from
//!    [`ctx.media`](ResolveCtx::media) or the client, exactly the
//!    wrapper's `getTmdbId` / `getTmdbNameAndYear` pair. Series titles
//!    carry the `S01E02` suffix, movies the `(year)`.
//! 2. **Server roster** — `GET api.wing.st/servers` (plain JSON),
//!    filtered to `status: "ok"`, cached 30 minutes with
//!    stale-while-error and a static 2026-09 fallback (`Lisbon`,
//!    `Nebula`, `Solara`, `Athens`); movie sweeps drop the anime-only
//!    `Sakura` server.
//! 3. **Seal** — for every server, build
//!    `{path: "/<server>/<movie|series>", payload: {tmdb, title, year,
//!    season?, episode?}}` and run it through the `crush.wasm`
//!    handshake, reimplemented below in pure Rust.
//! 4. **Parse** — the decrypted `data.stream` array holds `hls`
//!    playlist entries and per-quality `file` entries; internal ids
//!    (non-`http` playlists) become `cinejoy.pk/e/` iframes, which the
//!    wrapper's iframe guard drops — kept for structural parity.
//! 5. **Validate** — `/synthetic/` URLs are probed with a 4-byte
//!    `Range` GET and broken ones (the server "Expired" class) are
//!    dropped.
//! 6. **Convert** — through the shared Nuvio plumbing
//!    ([`crate::nuvio`]): unwrap the `pengu.uk` proxy frames, dedup by
//!    URL, build `[quality, WEB-DL, HEVC+HDR | x264, English]` marker
//!    titles, and attach the `cinejoy.pk` hotlink `Referer`.
//!
//! ## The `lumen-gate-v2` seal
//!
//! The `.cjs` delegates the crypto to `crush.wasm` (67 KB, embedded as
//! base64, rotated server-side). The module's `seal_request` export was
//! disassembled and the algorithm re-derived; this port reimplements it
//! without a WebAssembly runtime and pins it with two byte-exact golden
//! frames (one captured from the wasm itself, one from a live
//! round-trip against `api.wing.st`):
//!
//! 1. **nonce** — 44 fresh bytes; the last 12 are the AES-GCM IV.
//! 2. **ephemeral scalar** — SHA-256(`"lumen-gate-v2|ephemeral|" ‖
//!    nonce[..32] ‖ counter_be32`), read big-endian, the counter
//!    retried until the scalar lands in `(0, n)` of P-256.
//! 3. **ECDH** — P-256 over [`num_bigint`] (the workspace ships no
//!    `p256` crate): the ephemeral scalar times the server's static
//!    key; the x coordinate is the shared secret.
//! 4. **HKDF-SHA256** — `PRK = HMAC(salt = ephemeral_public, IKM =
//!    shared)`; one-block expands with `"lumen-gate-v2|c2s"` (request
//!    key) and `"lumen-gate-v2|s2c"` (response key).
//! 5. **request** — AES-256-GCM under the c2s key, IV = `nonce[32..44]`,
//!    AAD = `"lumen-gate-v2" ‖ [0x00, 0x01, key_id] ‖
//!    ephemeral_public`; the POST body is `[0x02, key_id, 0x04] ‖
//!    ephemeral_public[1..] ‖ IV ‖ ct ‖ tag`.
//! 6. **response** — `IV ‖ ct ‖ tag`, AES-256-GCM under the s2c key
//!    with the AAD direction byte `0x02`; the plaintext is
//!    `{"data":{"stream":[…]},"status":200}`.
//!
//! ## Cuts
//!
//! - Binary requests use `FetchRequest::post_bytes` and bounded raw responses
//!   use `Fetcher::probe`; no UTF-8 conversion touches encrypted frames.
//! - **`crush.wasm` execution** — no WASM runtime in the workspace (the
//!   same cut as `nuvio::flixcloud`); the algorithm is reimplemented and
//!   golden-pinned instead.
//! - **Key rotation.** The `.cjs` re-downloads `crush.wasm` (10-minute
//!   TTL) because the server rotates the static key; this port ships
//!   the current key parsed from the live module (see
//!   `SERVER_PUBLIC_KEY_HEX`). A rotation degrades to the upstream's
//!   stale-key behavior — every sealed request 404s, the sweep yields
//!   empty. The 404-retry-with-fresh-wasm leg is cut with it.
//! - **Anime detection.** The wrapper probes TMDB for
//!   `original_language == "ja"` + the animation genre;
//!   [`TmdbClient`] does not expose those fields, so the non-anime
//!   branch always applies (`English` marker, `[multi, en]` codes).
//! - **The scraper's own TMDB fetch** (`getTMDBInfo` with its hardcoded
//!   key and 10-second race) — the wrapper already resolved name/year,
//!   so the port reuses that.
//! - **The website-player iframe fallback** — the `.cjs` appends a
//!   `cinejoy.pk/e/` iframe when every server fails; the wrapper's
//!   iframe guard drops it immediately (net effect: empty), so it is
//!   not emitted.
//! - **`bingeGroup` / `proxyHeaders` behavior hints** — resolver-level
//!   display state the shared builder does not consume.
//! - **The `captions` arrays** — the wrapper never maps them to
//!   subtitle tracks.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fancy_regex::Regex;
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{
    BuildParams, NuvioStream, build_stream_results, parse_height, with_retry_on_empty,
};

// ---------------------------------------------------------------------------
// Constants — the upstream's single-file scraper, verbatim.
// ---------------------------------------------------------------------------

/// The provider id (`cinejoyaio`).
const ID: &str = "cinejoyaio";
/// The display label (`CineJoy` — also the scraper's `PROVIDER_NAME`).
const LABEL: &str = "CineJoy";
/// The sealed API root (2026-09-17: was `api.shegu.st`).
const API_BASE: &str = "https://api.wing.st";
/// The site origin (2026-09-17: was `cinejoy.to`).
const CINEJOY_ORIGIN: &str = "https://cinejoy.pk";
/// The browser `User-Agent` the scraper sends everywhere.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The static 2026-09 server roster (the live `/servers` answer's shape).
const FALLBACK_SERVERS: [&str; 4] = ["Lisbon", "Nebula", "Solara", "Athens"];
/// Result lifetime — the wrapper's `this.ttl` (10 minutes).
const TTL: Duration = Duration::from_secs(600);
/// The server-roster cache lifetime (30 minutes upstream).
const ROSTER_TTL: Duration = Duration::from_mins(30);
/// Per-stream country codes — the wrapper's non-anime branch (the
/// provider-level default also carries `ja`).
const COUNTRY_CODES: [CountryCode; 2] = [CountryCode::Multi, CountryCode::En];

/// The protocol name.
const PROTOCOL_PREFIX: &[u8] = b"lumen-gate-v2";
/// The HKDF info for the request key.
const C2S_INFO: &[u8] = b"lumen-gate-v2|c2s";
/// The HKDF info for the response key.
const S2C_INFO: &[u8] = b"lumen-gate-v2|s2c";
/// The scalar-derivation label.
const EPHEMERAL_INFO: &[u8] = b"lumen-gate-v2|ephemeral|";
/// The nonce length: 32 scalar-seed bytes + the 12-byte IV.
const NONCE_LEN: usize = 44;
/// The AES-GCM IV length.
const IV_LEN: usize = 12;
/// The AES-GCM tag length.
const TAG_LEN: usize = 16;

/// The server's static ECDH key id — the wasm output header pins it
/// alongside the key (the embedded fallback key carries id 1, the live
/// module 2; the server rotates both with the module).
const KEY_ID: u8 = 2;
/// The server's static ECDH key (65-byte uncompressed point), parsed on
/// 2026-09-25 from the live `crush.wasm` data segment — the only
/// `04 ‖ X ‖ Y` sequence on the P-256 curve in the module, and the key
/// whose sealed frames the live endpoint accepted.
const SERVER_PUBLIC_KEY_HEX: &str = "045c88a0ae33c683a4872590b04b22f4774a4fe1c6ecf74785c74a919a71207ca94f9e6b5a7854c3aa3b44ee46bc444fea694b9f23bcd80f864ecbef48c1ef6e14";
static SERVER_PUBLIC_KEY: LazyLock<Vec<u8>> = LazyLock::new(|| unhex(SERVER_PUBLIC_KEY_HEX));

/// Decode a hex string (byte pairs, order preserved). Invalid pairs
/// decode as `0` — every constant here is pinned by a golden-frame
/// test.
fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2).unwrap_or(""), 16).unwrap_or(0))
        .collect()
}

// ---------------------------------------------------------------------------
// P-256 — the one curve the handshake needs.
// ---------------------------------------------------------------------------

mod p256 {
    //! A minimal NIST P-256 over [`BigUint`].
    //!
    //! Jacobian coordinates (`X, Y, Z` ↔ affine `X/Z², Y/Z³`; `Z == 0`
    //! is the point at infinity), doubling per EFD `dbl-2001-b`
    //! (a = −3), addition per `add-2007-bl`, MSB-first
    //! double-and-add. Not constant-time — the scalar is per-request
    //! random, there is no long-lived secret to protect.

    use std::sync::LazyLock;

    use num_bigint::BigUint;

    /// The field prime p = 2²⁵⁶ − 2²²⁴ + 2¹⁹² + 2⁹⁶ − 1.
    static P: LazyLock<BigUint> =
        LazyLock::new(|| big("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff"));
    /// The group order n.
    static N: LazyLock<BigUint> =
        LazyLock::new(|| big("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551"));
    /// The curve coefficient b.
    static B: LazyLock<BigUint> =
        LazyLock::new(|| big("5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b"));
    /// The base point x.
    static GX: LazyLock<BigUint> =
        LazyLock::new(|| big("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296"));
    /// The base point y.
    static GY: LazyLock<BigUint> =
        LazyLock::new(|| big("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"));

    /// The group order.
    pub(super) fn order() -> &'static BigUint {
        &N
    }

    /// Parse a big-endian hex integer.
    fn big(hex: &str) -> BigUint {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(hex.get(index..index + 2).unwrap_or(""), 16).unwrap_or(0)
            })
            .collect();
        BigUint::from_bytes_be(&bytes)
    }

    /// (a + b) mod p.
    fn add(a: &BigUint, b: &BigUint) -> BigUint {
        (a + b) % &*P
    }

    /// (a − b) mod p (underflow-safe).
    fn sub(a: &BigUint, b: &BigUint) -> BigUint {
        if a >= b {
            (a - b) % &*P
        } else {
            (a + &*P - b) % &*P
        }
    }

    /// (a · b) mod p.
    fn mul(a: &BigUint, b: &BigUint) -> BigUint {
        (a * b) % &*P
    }

    /// Whether the value is zero (`bits() == 0`).
    fn is_zero(value: &BigUint) -> bool {
        value.bits() == 0
    }

    /// a⁻¹ mod p (Fermat — p is prime).
    fn inv(a: &BigUint) -> BigUint {
        let exponent = &*P - BigUint::from(2u32);
        a.modpow(&exponent, &P)
    }

    /// Whether (x, y) satisfies y² = x³ − 3x + b.
    fn on_curve(x: &BigUint, y: &BigUint) -> bool {
        let three = BigUint::from(3u32);
        let left = mul(y, y);
        let x_cubed = mul(&mul(x, x), x);
        let right = add(&sub(&x_cubed, &mul(x, &three)), &B);
        left == right
    }

    /// A point in Jacobian coordinates.
    #[derive(Clone)]
    struct Jacobian {
        x: BigUint,
        y: BigUint,
        z: BigUint,
    }

    /// The point at infinity.
    fn identity() -> Jacobian {
        Jacobian {
            x: BigUint::from(0u32),
            y: BigUint::from(0u32),
            z: BigUint::from(0u32),
        }
    }

    /// EFD `dbl-2001-b` (a = −3): `M = 3X² − 3Z⁴`, `S = 4XY²`,
    /// `X' = M² − 2S`, `Y' = M(S − X') − 8Y⁴`, `Z' = 2YZ`.
    fn double(point: &Jacobian) -> Jacobian {
        if is_zero(&point.z) || is_zero(&point.y) {
            return identity();
        }
        let three = BigUint::from(3u32);
        let y_squared = mul(&point.y, &point.y);
        let s = mul(&mul(&point.x, &y_squared), &BigUint::from(4u32));
        let z_squared = mul(&point.z, &point.z);
        let m = sub(
            &mul(&mul(&point.x, &point.x), &three),
            &mul(&mul(&z_squared, &z_squared), &three),
        );
        let x_prime = sub(&mul(&m, &m), &add(&s, &s));
        let y_fourth = mul(&y_squared, &y_squared);
        let y_prime = sub(
            &mul(&m, &sub(&s, &x_prime)),
            &mul(&y_fourth, &BigUint::from(8u32)),
        );
        let z_prime = mul(&mul(&point.y, &point.z), &BigUint::from(2u32));
        Jacobian {
            x: x_prime,
            y: y_prime,
            z: z_prime,
        }
    }

    /// EFD `add-2007-bl` — the identifiers are the formula's own.
    #[allow(clippy::many_single_char_names)]
    fn add_points(p: &Jacobian, q: &Jacobian) -> Jacobian {
        if is_zero(&p.z) {
            return q.clone();
        }
        if is_zero(&q.z) {
            return p.clone();
        }
        let z1z1 = mul(&p.z, &p.z);
        let z2z2 = mul(&q.z, &q.z);
        let u1 = mul(&p.x, &z2z2);
        let u2 = mul(&q.x, &z1z1);
        let s1 = mul(&mul(&p.y, &z2z2), &q.z);
        let s2 = mul(&mul(&q.y, &z1z1), &p.z);
        if u1 == u2 {
            if s1 == s2 {
                return double(p);
            }
            return identity();
        }
        let h = sub(&u2, &u1);
        let r = sub(&s2, &s1);
        let h_squared = mul(&h, &h);
        let h_cubed = mul(&h, &h_squared);
        let v = mul(&u1, &h_squared);
        let x_prime = sub(&sub(&mul(&r, &r), &h_cubed), &add(&v, &v));
        let y_prime = sub(&mul(&r, &sub(&v, &x_prime)), &mul(&s1, &h_cubed));
        let z_prime = mul(&mul(&p.z, &q.z), &h);
        Jacobian {
            x: x_prime,
            y: y_prime,
            z: z_prime,
        }
    }

    /// The scalar multiple `scalar · base` (MSB-first).
    fn scalar_mult(scalar: &BigUint, base: &Jacobian) -> Jacobian {
        let bytes = scalar.to_bytes_be();
        let bits = usize::try_from(scalar.bits()).unwrap_or(usize::MAX);
        let mut acc = identity();
        for position in (0..bits).rev() {
            acc = double(&acc);
            let from_end = position / 8;
            let Some(byte) = bytes.get(bytes.len() - 1 - from_end) else {
                continue;
            };
            if (byte >> (position % 8)) & 1 == 1 {
                acc = add_points(&acc, base);
            }
        }
        acc
    }

    /// The affine form — `None` for the identity.
    fn to_affine(point: &Jacobian) -> Option<(BigUint, BigUint)> {
        if is_zero(&point.z) {
            return None;
        }
        let z_inv = inv(&point.z);
        let z_inv_squared = mul(&z_inv, &z_inv);
        let z_inv_cubed = mul(&z_inv_squared, &z_inv);
        Some((mul(&point.x, &z_inv_squared), mul(&point.y, &z_inv_cubed)))
    }

    /// Decode a 65-byte uncompressed point (`04 ‖ X ‖ Y`), checking the
    /// range and the curve equation.
    pub(super) fn decode_point(bytes: &[u8]) -> Option<(BigUint, BigUint)> {
        if bytes.len() != 65 || bytes.first() != Some(&0x04) {
            return None;
        }
        let x = BigUint::from_bytes_be(bytes.get(1..33)?);
        let y = BigUint::from_bytes_be(bytes.get(33..65)?);
        if x >= *P || y >= *P || !on_curve(&x, &y) {
            return None;
        }
        Some((x, y))
    }

    /// The public key for a scalar — `04 ‖ X ‖ Y`, big-endian.
    pub(super) fn public_key(scalar: &BigUint) -> Option<[u8; 65]> {
        let base = Jacobian {
            x: GX.clone(),
            y: GY.clone(),
            z: BigUint::from(1u32),
        };
        let (x, y) = to_affine(&scalar_mult(scalar, &base))?;
        let mut out = [0u8; 65];
        out[0] = 0x04;
        out[1..33].copy_from_slice(&to_fixed_be(&x, 32)?);
        out[33..65].copy_from_slice(&to_fixed_be(&y, 32)?);
        Some(out)
    }

    /// The ECDH shared secret — the x coordinate of `scalar · point`.
    pub(super) fn shared_x(scalar: &BigUint, point: (BigUint, BigUint)) -> Option<[u8; 32]> {
        let base = Jacobian {
            x: point.0,
            y: point.1,
            z: BigUint::from(1u32),
        };
        let (x, _) = to_affine(&scalar_mult(scalar, &base))?;
        let bytes = to_fixed_be(&x, 32)?;
        bytes.try_into().ok()
    }

    /// Big-endian bytes left-padded to `len`.
    fn to_fixed_be(value: &BigUint, len: usize) -> Option<Vec<u8>> {
        let mut bytes = value.to_bytes_be();
        if bytes.len() > len {
            return None;
        }
        let mut out = vec![0u8; len - bytes.len()];
        out.append(&mut bytes);
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// AES-256-GCM over the raw block cipher (SP 800-38D, with AAD).
// ---------------------------------------------------------------------------

mod gcm {
    //! AES-256-GCM seal + open with associated data — the same
    //! shift-and-XOR construction the `VidZee` extractor uses, extended
    //! with AAD and an encryption direction.

    use aes::Aes256;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};

    /// The GHASH reduction constant R = E1 ‖ 0^120.
    const R: u128 = 0xE1 << 120;
    /// The key length.
    const KEY: usize = 32;
    /// The IV length.
    const IV: usize = 12;
    /// The tag length.
    const TAG: usize = 16;

    /// The GF(2^128) product used by GHASH (SP 800-38D Algorithm 1).
    ///
    /// GCM's bit order is reflected — the leftmost block bit is the
    /// coefficient of x⁰ — so the multiplier's bits are consumed from
    /// the top while `v` is multiplied by x (a right shift in integer
    /// terms, reducing x¹²⁸ with `R` whenever the bottom bit leaves).
    fn gf_mul(x: u128, y: u128) -> u128 {
        let mut z: u128 = 0;
        let mut v = y;
        let mut x = x;
        for _ in 0..128 {
            if x >> 127 == 1 {
                z ^= v;
            }
            x <<= 1;
            let lsb = v & 1;
            v >>= 1;
            if lsb == 1 {
                v ^= R;
            }
        }
        z
    }

    /// GHASH over (AAD, data): per-block multiplication, then the
    /// bit-length block `[len(AAD)][len(data)]`.
    fn ghash(h: [u8; 16], aad: &[u8], data: &[u8]) -> [u8; 16] {
        let h = u128::from_be_bytes(h);
        let mut y: u128 = 0;
        let mut block = [0u8; 16];
        for chunks in [aad, data] {
            for chunk in chunks.chunks(16) {
                block.fill(0);
                block[..chunk.len()].copy_from_slice(chunk);
                y = gf_mul(y ^ u128::from_be_bytes(block), h);
            }
        }
        block.fill(0);
        block[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        block[8..].copy_from_slice(&((data.len() as u64) * 8).to_be_bytes());
        gf_mul(y ^ u128::from_be_bytes(block), h).to_be_bytes()
    }

    /// `E_K(block)`, in place.
    fn e_k(cipher: &Aes256, block: &mut [u8; 16]) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }

    /// inc32 — the big-endian counter in the last four bytes.
    fn increment(counter: &mut [u8; 16]) {
        for byte in counter[12..].iter_mut().rev() {
            let (value, overflow) = byte.overflowing_add(1);
            *byte = value;
            if !overflow {
                break;
            }
        }
    }

    /// The CTR keystream starting at inc32(J0).
    fn keystream(cipher: &Aes256, j0: [u8; 16], length: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(length);
        let mut counter = j0;
        while out.len() < length {
            increment(&mut counter);
            let mut block = counter;
            e_k(cipher, &mut block);
            let take = (length - out.len()).min(16);
            out.extend_from_slice(&block[..take]);
        }
        out
    }

    /// The J0 pre-image: IV ‖ 0^31 ‖ 1.
    fn j0(iv: &[u8]) -> Option<[u8; 16]> {
        if iv.len() != IV {
            return None;
        }
        let mut j0 = [0u8; 16];
        j0[..IV].copy_from_slice(iv);
        j0[15] = 1;
        Some(j0)
    }

    /// Encrypt `plaintext` under `key`/`iv` with `aad`, returning
    /// `(ciphertext, tag)`.
    pub(super) fn seal(
        key: &[u8],
        iv: &[u8],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Option<(Vec<u8>, [u8; 16])> {
        if key.len() != KEY {
            return None;
        }
        let cipher = Aes256::new(GenericArray::from_slice(key));
        let mut h = [0u8; 16];
        e_k(&cipher, &mut h);
        let j0 = j0(iv)?;
        let stream = keystream(&cipher, j0, plaintext.len());
        let ciphertext: Vec<u8> = plaintext
            .iter()
            .zip(stream)
            .map(|(byte, key_byte)| byte ^ key_byte)
            .collect();
        let s = ghash(h, aad, &ciphertext);
        let mut tag = j0;
        e_k(&cipher, &mut tag);
        for (tag_byte, s_byte) in tag.iter_mut().zip(s) {
            *tag_byte ^= s_byte;
        }
        Some((ciphertext, tag))
    }

    /// Decrypt-and-verify — `None` on any length error or tag mismatch
    /// (the upstream `decipher.final()` throw).
    pub(super) fn open(
        key: &[u8],
        iv: &[u8],
        aad: &[u8],
        ciphertext: &[u8],
        tag: &[u8],
    ) -> Option<Vec<u8>> {
        if key.len() != KEY || tag.len() != TAG {
            return None;
        }
        let cipher = Aes256::new(GenericArray::from_slice(key));
        let mut h = [0u8; 16];
        e_k(&cipher, &mut h);
        let j0 = j0(iv)?;
        let s = ghash(h, aad, ciphertext);
        let mut expected = j0;
        e_k(&cipher, &mut expected);
        for (expected_byte, s_byte) in expected.iter_mut().zip(s) {
            *expected_byte ^= s_byte;
        }
        if !constant_time_eq(&expected, tag) {
            return None;
        }
        let stream = keystream(&cipher, j0, ciphertext.len());
        Some(
            ciphertext
                .iter()
                .zip(stream)
                .map(|(byte, key_byte)| byte ^ key_byte)
                .collect(),
        )
    }

    /// A constant-time tag comparison.
    fn constant_time_eq(a: &[u8; 16], b: &[u8]) -> bool {
        b.len() == TAG && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

// ---------------------------------------------------------------------------
// lumen-gate-v2: seal + response decrypt.
// ---------------------------------------------------------------------------

/// HMAC-SHA256 over the `hmac` crate.
fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; 32]> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(message);
    let bytes: [u8; 32] = mac.finalize().into_bytes().as_slice().try_into().ok()?;
    Some(bytes)
}

/// HKDF-Extract plus a single Expand block (RFC 5869) — both infos ask
/// for exactly 32 bytes.
fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8]) -> Option<[u8; 32]> {
    let prk = hmac_sha256(salt, ikm)?;
    let mut message = info.to_vec();
    message.push(1);
    hmac_sha256(&prk, &message)
}

/// The sealed request frame and the material the response needs.
#[derive(Debug)]
struct Sealed {
    /// The s2c key (`HKDF(…|s2c)`) — decrypts the reply.
    response_key: [u8; 32],
    /// The key id the frame was sealed under.
    key_id: u8,
    /// The ephemeral public key (`04 ‖ X ‖ Y`).
    ephemeral_public: [u8; 65],
    /// The POST body: `[0x02, key_id, 0x04] ‖ eph[1..] ‖ IV ‖ ct ‖ tag`.
    body: Vec<u8>,
}

/// The ephemeral scalar — SHA-256 of the label, the first 32 nonce
/// bytes and a big-endian counter, retried until it lands in `(0, n)`.
fn derive_ephemeral_scalar(nonce: &[u8]) -> Option<BigUint> {
    for counter in 0..=255u32 {
        let mut hash = Sha256::new();
        hash.update(EPHEMERAL_INFO);
        hash.update(&nonce[..32]);
        hash.update(counter.to_be_bytes());
        let digest: [u8; 32] = hash.finalize().into();
        let scalar = BigUint::from_bytes_be(&digest);
        if scalar > BigUint::from(0u32) && scalar < *p256::order() {
            return Some(scalar);
        }
    }
    None
}

/// The GCM associated data: the protocol name, a zero byte, the
/// direction (`0x01` request / `0x02` response), the key id, and the
/// ephemeral public key.
fn build_aad(direction: u8, key_id: u8, ephemeral_public: &[u8; 65]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(13 + 3 + 65);
    aad.extend_from_slice(PROTOCOL_PREFIX);
    aad.push(0x00);
    aad.push(direction);
    aad.push(key_id);
    aad.extend_from_slice(ephemeral_public);
    aad
}

/// Seal a request payload — the pure-Rust `crush.wasm`
/// `seal_request`, byte-pinned by the golden-frame tests.
///
/// `nonce` holds the 44 fresh bytes; `server_public` the 65-byte
/// static key. Returns the wire body plus the response key material.
fn seal_request(payload: &[u8], nonce: &[u8], key_id: u8, server_public: &[u8]) -> Option<Sealed> {
    if nonce.len() != NONCE_LEN {
        return None;
    }
    let scalar = derive_ephemeral_scalar(nonce)?;
    let ephemeral_public = p256::public_key(&scalar)?;
    let shared = p256::shared_x(&scalar, p256::decode_point(server_public)?)?;
    let request_key = hkdf_sha256(&ephemeral_public, &shared, C2S_INFO)?;
    let response_key = hkdf_sha256(&ephemeral_public, &shared, S2C_INFO)?;
    let iv = &nonce[32..44];
    let aad = build_aad(0x01, key_id, &ephemeral_public);
    let (ciphertext, tag) = gcm::seal(&request_key, iv, &aad, payload)?;
    let mut body = Vec::with_capacity(3 + 64 + IV_LEN + ciphertext.len() + TAG_LEN);
    body.extend_from_slice(&[0x02, key_id, 0x04]);
    body.extend_from_slice(&ephemeral_public[1..]);
    body.extend_from_slice(iv);
    body.extend_from_slice(&ciphertext);
    body.extend_from_slice(&tag);
    Some(Sealed {
        response_key,
        key_id,
        ephemeral_public,
        body,
    })
}

/// Decrypt a `/g` response frame (`IV ‖ ct ‖ tag`) into its JSON —
/// the port of `decryptResponse` (the tag mismatch is the upstream
/// `decipher.final()` throw).
fn decrypt_response(raw: &[u8], sealed: &Sealed) -> Option<Value> {
    if raw.len() < IV_LEN + TAG_LEN {
        return None;
    }
    let (iv, rest) = raw.split_at(IV_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
    let aad = build_aad(0x02, sealed.key_id, &sealed.ephemeral_public);
    let plain = gcm::open(&sealed.response_key, iv, &aad, ciphertext, tag)?;
    serde_json::from_slice(&plain).ok()
}

/// The 44 fresh nonce bytes — a hash of the clock, a process-wide
/// counter and the pid. AES-GCM needs a *unique* IV, not a secret one,
/// and the counter guarantees uniqueness within the process; the port
/// has no `rand` crate to draw from.
fn random_nonce() -> [u8; NONCE_LEN] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut first = Sha256::new();
    first.update(nanos.to_be_bytes());
    first.update(count.to_be_bytes());
    first.update(u64::from(std::process::id()).to_be_bytes());
    let first: [u8; 32] = first.finalize().into();
    let mut second = Sha256::new();
    second.update(first);
    second.update(count.wrapping_add(1).to_be_bytes());
    let second: [u8; 32] = second.finalize().into();
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..32].copy_from_slice(&first);
    nonce[32..].copy_from_slice(&second[..IV_LEN]);
    nonce
}

// ---------------------------------------------------------------------------
// Request building + stream parsing (the .cjs half).
// ---------------------------------------------------------------------------

/// The request context threaded through sealing and parsing — the
/// upstream's `tmdbInfo` object (`title`, `year`, `tmdbId`, `type`,
/// `season`, `episode`).
#[derive(Debug, Clone)]
struct RequestInfo {
    /// The TMDB id as a string (the payload carries strings).
    tmdb_id: String,
    /// Whether this is an episodic request (`type === 'tv'`).
    is_tv: bool,
    /// The season number.
    season: Option<u32>,
    /// The episode number.
    episode: Option<u32>,
    /// The TMDB title.
    title: String,
    /// The release year, when known.
    year: Option<String>,
}

/// Build the sealed payload for one server — the port of
/// `buildRequest`: a clean path (`/<server>/<movie|series>`) with the
/// parameters inside the sealed JSON (both values strings, season and
/// episode only when both are present).
fn build_request(server: &str, info: &RequestInfo) -> Option<String> {
    let media = if info.is_tv { "series" } else { "movie" };
    let mut payload = serde_json::Map::new();
    payload.insert("tmdb".to_string(), json!(info.tmdb_id));
    if !info.title.is_empty() {
        payload.insert("title".to_string(), json!(info.title));
    }
    if let Some(year) = info.year.as_deref().filter(|year| !year.is_empty()) {
        payload.insert("year".to_string(), json!(year));
    }
    if let (Some(season), Some(episode)) = (info.season, info.episode) {
        payload.insert("season".to_string(), json!(season.to_string()));
        payload.insert("episode".to_string(), json!(episode.to_string()));
    }
    let frame = json!({
        "path": format!("/{server}/{media}"),
        "payload": Value::Object(payload),
    });
    serde_json::to_string(&frame).ok()
}

/// The `cinejoy.pk/e/` embed page for internal-id entries — the
/// upstream's client-side resolution fallback.
fn embed_url(info: &RequestInfo) -> String {
    let kind = if info.is_tv { "tv" } else { "movie" };
    if info.is_tv {
        format!(
            "{CINEJOY_ORIGIN}/e/{kind}/{}/{}/{}",
            info.tmdb_id,
            info.season.unwrap_or(1),
            info.episode.unwrap_or(1)
        )
    } else {
        format!("{CINEJOY_ORIGIN}/e/{kind}/{}", info.tmdb_id)
    }
}

/// One raw stream as the scraper emits it — the fields the wrapper
/// actually consumes (its conversion drops the `.cjs` MIME hint; the
/// shared builder infers the format from the URL).
#[derive(Debug, Clone)]
struct RawStream {
    /// The stream or embed URL.
    url: String,
    /// `CineJoy - <server>[ (id)]` / `CineJoy - <server> <quality> (id)` —
    /// the wrapper extracts the server name from it.
    name: String,
    /// The quality label (`2160p`, `1080p`, …).
    quality: Option<String>,
    /// Whether this is an iframe entry (the wrapper skips those).
    is_iframe: bool,
}

/// Parse one entry of the decrypted `data.stream` array — the port of
/// `parseStreamEntry`.
///
/// Entries are either `{type: "hls", id, playlist, captions}` or
/// `{type: "file", id, qualities: {<q>: {type, url}}, captions}`;
/// non-`http` playlists/urls are internal ids that become embed-page
/// iframes (upstream marks them `notWebVideo`; the wrapper drops
/// them, so they only round out the structure).
fn parse_stream_entry(entry: &Value, server: &str, info: &RequestInfo) -> Vec<RawStream> {
    let mut out = Vec::new();
    let kind = entry.get("type").and_then(Value::as_str);
    let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
    if kind == Some("hls") {
        if let Some(playlist) = entry
            .get("playlist")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
        {
            let name = format!(
                "{LABEL} - {server}{}",
                if id.is_empty() {
                    String::new()
                } else {
                    format!(" ({id})")
                }
            );
            if playlist.starts_with("http") {
                out.push(RawStream {
                    url: playlist.to_string(),
                    name,
                    // Lisbon carries the 4K HDR tier; every other server answers 1080p.
                    quality: Some(if server == "Lisbon" { "2160p" } else { "1080p" }.to_string()),
                    is_iframe: false,
                });
            } else {
                out.push(RawStream {
                    url: embed_url(info),
                    name,
                    quality: Some("1080p".to_string()),
                    is_iframe: true,
                });
            }
        }
    } else if kind == Some("file")
        && let Some(qualities) = entry.get("qualities").and_then(Value::as_object)
    {
        // `serde_json` walks the map in sorted order where the JS
        // walks insertion order — only the stream sequence differs.
        for (quality, quality_info) in qualities {
            let Some(url) = quality_info
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
            else {
                continue;
            };
            let name = format!("{LABEL} - {server} {quality} ({id})");
            let label = if quality == "unknown" {
                "720p".to_string()
            } else {
                format!("{quality}p")
            };
            if url.starts_with("http") {
                out.push(RawStream {
                    url: url.to_string(),
                    name,
                    quality: Some(label),
                    is_iframe: false,
                });
            } else {
                out.push(RawStream {
                    url: embed_url(info),
                    name,
                    quality: Some(label),
                    is_iframe: true,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The wrapper half: pengu unwrap + conversion into Nuvio streams.
// ---------------------------------------------------------------------------

/// The `pengu.uk` resource segment (`/resource/{base64url}`).
static PENGU_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/resource/([^/]+)").unwrap_or_else(|_| panic!("invalid pengu regex"))
});
/// The server-name token in a display title (`[Lisbon]`).
static SERVER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(\w+)\]").unwrap_or_else(|_| panic!("invalid server-token regex"))
});

/// Unwrap a `pengu.uk/hls/cinejoy/resource/{base64url}` frame to the
/// original URL plus its `Referer` — the port of `unwrapPenguProxy`.
///
/// Non-proxy URLs and undecodable frames fall back to the URL as-is
/// with the `cinejoy.pk` referer (the JS does the same).
fn unwrap_pengu_proxy(url: &str) -> (String, String) {
    let default_referer = format!("{CINEJOY_ORIGIN}/");
    if !url.contains("pengu.uk/") {
        return (url.to_string(), default_referer);
    }
    if let Some(captures) = PENGU_RESOURCE.captures(url).ok().flatten() {
        let encoded = captures
            .get(1)
            .map(|group| group.as_str())
            .unwrap_or_default();
        // Node's `base64url` accepts missing padding; strip it before
        // the unpadded engine.
        let decoded = URL_SAFE_NO_PAD.decode(encoded.trim_end_matches('=')).ok();
        let data = decoded
            .as_deref()
            .and_then(|raw| serde_json::from_slice::<Value>(raw).ok());
        if let Some(data) = data {
            let url = data
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map_or_else(|| url.to_string(), str::to_string);
            let referer = data
                .pointer("/headers/Referer")
                .and_then(Value::as_str)
                .filter(|referer| !referer.is_empty())
                .map_or_else(|| default_referer, str::to_string);
            return (url, referer);
        }
    }
    (url.to_string(), default_referer)
}

/// The server name hidden in a scraper stream name —
/// `CineJoy - Lisbon 1080p (primary)` → `Lisbon` (the JS
/// `split(' - ')[1].split(' ')[0] || 'CineJoy'`).
fn server_of_name(name: &str) -> String {
    name.split(" - ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .filter(|token| !token.is_empty())
        .map_or_else(|| LABEL.to_string(), str::to_string)
}

/// Convert raw scraper streams into [`NuvioStream`]s — the port of the
/// wrapper's conversion loop: skip iframes, unwrap the pengu proxy,
/// build the marker title, dedup by URL.
fn convert_streams(raw: &[RawStream], title: &str) -> Vec<NuvioStream> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for stream in raw {
        if stream.is_iframe {
            // The wrapper's `type === 'iframe' || notWebVideo` guard.
            continue;
        }
        let (raw_url, referer) = unwrap_pengu_proxy(&stream.url);
        if !raw_url.starts_with("http") {
            continue;
        }
        let height = parse_height(stream.quality.as_deref());
        let server = server_of_name(&stream.name);
        // The marker title: quality, WEB-DL, codec tier, language.
        let mut markers: Vec<String> = Vec::new();
        if let Some(quality) = stream
            .quality
            .as_deref()
            .filter(|quality| !quality.is_empty())
        {
            markers.push(quality.to_string());
        }
        markers.push("WEB-DL".to_string());
        if height == Some(2160) {
            markers.push("HEVC".to_string());
            markers.push("HDR".to_string());
        } else {
            markers.push("x264".to_string());
        }
        // The anime branch is cut (no original language in TmdbClient).
        markers.push("English".to_string());
        let display_title = format!("{title} [{server}] {}", markers.join(" "));
        // The wrapper re-extracts the server token from the title it
        // just built (`title.match(/\[(\w+)\]/)`), falling back to
        // `Stream` — warts included, the title itself may carry
        // brackets.
        let token = SERVER_TOKEN
            .captures(&display_title)
            .ok()
            .flatten()
            .and_then(|captures| captures.get(1))
            .map_or_else(|| "Stream".to_string(), |group| group.as_str().to_string());
        if !seen.insert(raw_url.clone()) {
            continue;
        }
        out.push(
            NuvioStream::new(raw_url)
                .with_quality(height.map_or_else(|| "1080p".to_string(), |h| format!("{h}p")))
                .with_title(display_title)
                .with_name(format!("{LABEL} - {token}"))
                .with_header("Referer", referer),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// The server roster (GET /servers — the one text endpoint).
// ---------------------------------------------------------------------------

/// The `/servers` answer.
#[derive(Deserialize)]
struct ServersResponse {
    /// The roster.
    servers: Vec<ServerEntry>,
}

/// One roster entry.
#[derive(Deserialize)]
struct ServerEntry {
    /// The server name.
    name: Option<String>,
    /// The health status (`ok` serves).
    status: Option<String>,
}

/// Fetch the live roster — the port of `getServers`: `status: "ok"`
/// entries only, `None` on any transport or shape failure so the
/// caller can fall back to the cache and then the static list.
async fn fetch_roster(ctx: &ResolveCtx<'_>) -> Option<Vec<String>> {
    let url = Url::parse(&format!("{API_BASE}/servers")).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Origin", CINEJOY_ORIGIN)
        .with_header("Referer", format!("{CINEJOY_ORIGIN}/"))
        .with_timeout(Duration::from_secs(8));
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let parsed: ServersResponse = response.json().ok()?;
    Some(
        parsed
            .servers
            .into_iter()
            .filter(|entry| {
                entry.name.as_deref().is_some_and(|name| !name.is_empty())
                    && entry.status.as_deref() == Some("ok")
            })
            .filter_map(|entry| entry.name)
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// The sealed sweep.
// ---------------------------------------------------------------------------

/// POST an encrypted binary frame and preserve the response bytes.
async fn sealed_post(ctx: &ResolveCtx<'_>, sealed: &Sealed) -> Option<Vec<u8>> {
    let url = Url::parse(&format!("{API_BASE}/g")).ok()?;
    let request = FetchRequest::post_bytes(url, sealed.body.clone())
        .with_header("Content-Type", "application/octet-stream")
        .with_header("Accept", "application/octet-stream")
        .with_header("Origin", "https://cinejoy.pk")
        .with_header("Referer", "https://cinejoy.pk/")
        .with_timeout(Duration::from_secs(12));
    let response = ctx.fetcher.probe(request, 2 * 1024 * 1024).await.ok()??;
    ((200..300).contains(&response.status) && !response.truncated).then_some(response.body)
}

/// One server's sealed round-trip — the port of `fetchFromServer`
/// (without its 404-refresh leg: key rotation is cut with the wasm
/// download).
async fn fetch_from_server(ctx: &ResolveCtx<'_>, payload: &[u8]) -> Option<Value> {
    let sealed = seal_request(payload, &random_nonce(), KEY_ID, &SERVER_PUBLIC_KEY)?;
    // The .cjs sanity-checks the wasm's output length before posting;
    // the port checks the frame's minimum extent (prefix, key
    // material, IV, tag).
    if sealed.body.len() < 3 + 64 + IV_LEN + TAG_LEN {
        return None;
    }
    let raw = sealed_post(ctx, &sealed).await?;
    decrypt_response(&raw, &sealed)
}

/// Probe a `/synthetic/` URL — the port of `validateStreamUrl`: a
/// 4-byte `Range` GET must succeed with a non-empty body (the
/// upstream's `#EXTM3U` check is subsumed by the non-empty one).
async fn validate_stream_url(ctx: &ResolveCtx<'_>, url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Range", "bytes=0-3")
        .with_timeout(Duration::from_secs(8));
    let Ok(response) = ctx.fetcher.request(request).await else {
        return false;
    };
    response.is_success() && !response.body.trim_start().is_empty()
}

/// Drop the broken synthetic URLs — the port of the validation sweep.
async fn validate_synthetic(ctx: &ResolveCtx<'_>, streams: &mut Vec<RawStream>) {
    let mut keep = Vec::with_capacity(streams.len());
    for stream in streams.drain(..) {
        if stream.url.contains("/synthetic/") && !validate_stream_url(ctx, &stream.url).await {
            continue;
        }
        keep.push(stream);
    }
    *streams = keep;
}

/// The full server sweep — the port of the `.cjs` `getStreams` main
/// body: one sealed request per server, `status: 200` replies parsed
/// into raw streams, then the synthetic-URL validation.
///
/// The JS fans the servers out with `Promise.all`; this port sweeps
/// sequentially — cross-provider fan-out is the engine's domain (the
/// same note as `cinewave`).
async fn sweep(ctx: &ResolveCtx<'_>, info: &RequestInfo, servers: &[String]) -> Vec<RawStream> {
    let mut all: Vec<RawStream> = Vec::new();
    for server in servers {
        let Some(request) = build_request(server, info) else {
            continue;
        };
        let Some(reply) = fetch_from_server(ctx, request.as_bytes()).await else {
            continue;
        };
        if reply.get("status").and_then(Value::as_i64) != Some(200) {
            continue;
        }
        if let Some(entries) = reply.pointer("/data/stream").and_then(Value::as_array) {
            for entry in entries {
                all.extend(parse_stream_entry(entry, server, info));
            }
        }
    }
    validate_synthetic(ctx, &mut all).await;
    all
}

// ---------------------------------------------------------------------------
// The provider.
// ---------------------------------------------------------------------------

/// The `CineJoy` provider (id `cinejoyaio`) — the all-in-one scraper
/// behind `cinejoy.pk`.
pub struct CineJoyAllInOne {
    info: SourceInfo,
    tmdb: Arc<TmdbClient>,
    roster: Mutex<Option<(Instant, Vec<String>)>>,
}

impl CineJoyAllInOne {
    /// Create the provider over a shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En, CountryCode::Ja],
                base_url: Url::parse(CINEJOY_ORIGIN).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            roster: Mutex::new(None),
        }
    }

    /// The server roster — live `/servers` with the 30-minute cache,
    /// stale-while-error, and the static fallback (the port of
    /// `getServers`).
    async fn roster(&self, ctx: &ResolveCtx<'_>) -> Vec<String> {
        {
            let guard = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, cached)) = guard.as_ref()
                && at.elapsed() < ROSTER_TTL
            {
                return cached.clone();
            }
        }
        let fetched = fetch_roster(ctx).await;
        let mut guard = self
            .roster
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match fetched.filter(|names| !names.is_empty()) {
            Some(names) => {
                *guard = Some((Instant::now(), names.clone()));
                names
            }
            // The stale cache beats the fallback; the fallback beats nothing.
            None => match guard.as_ref() {
                Some((_, stale)) => stale.clone(),
                None => FALLBACK_SERVERS
                    .iter()
                    .map(|server| (*server).to_string())
                    .collect(),
            },
        }
    }
}

/// The name/year pair from `ctx.media` or the TMDB client — the
/// wrapper's `getTmdbNameAndYear` (pre-resolved metadata first).
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

#[async_trait]
impl Source for CineJoyAllInOne {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // The wrapper resolves the TMDB id first (IMDb references map
        // through `/find`).
        let tmdb_id = match &media.id {
            MediaId::Tmdb(id) => *id,
            MediaId::Imdb(imdb) => self.tmdb.tmdb_id_from_imdb(imdb, media.kind).await?,
        };
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;

        // An episodic request needs both numbers — the .cjs bails early
        // because the server rejects anything else.
        let is_tv = media.season.is_some();
        if is_tv && media.episode.is_none() {
            return Ok(Vec::new());
        }
        // The display title: `Name S01E02` for episodes, `Name (year)`
        // for movies (a missing year keeps the bare name — the JS
        // prints `(undefined)`).
        let title = match (media.season, media.episode) {
            (Some(season), Some(episode)) => format!("{name} S{season:02}E{episode:02}"),
            _ => year.map_or_else(|| name.clone(), |year| format!("{name} ({year})")),
        };
        let info = RequestInfo {
            tmdb_id: tmdb_id.to_string(),
            is_tv,
            season: media.season,
            episode: media.episode,
            title: name,
            year: year.map(|year| year.to_string()),
        };

        // The roster: live, cached, or fallback; movies drop Sakura.
        let roster = self.roster(ctx).await;
        let servers: Vec<String> = if is_tv {
            roster
        } else {
            roster
                .into_iter()
                .filter(|server| server != "Sakura")
                .collect()
        };

        // The wrapper's one empty-retry: a fast empty first attempt
        // (the cold wasm window upstream) sleeps 1.5s and tries again.
        let raw = with_retry_on_empty(
            || async { Some(sweep(ctx, &info, &servers).await) },
            2,
            Duration::from_secs(16),
            Duration::from_millis(1500),
        )
        .await
        .unwrap_or_default();

        let converted = convert_streams(&raw, &title);
        Ok(build_stream_results(&BuildParams {
            streams: &converted,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &COUNTRY_CODES,
            ttl: TTL,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests — golden frames from the live protocol, parsing, conversion,
// and the graceful degradation of the transport cut.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchResponse, Fetcher};

    use super::*;

    /// The Inception request both golden frames sealed.
    const PAYLOAD: &[u8] =
        br#"{"path":"/Lisbon/movie","payload":{"tmdb":"27205","title":"Inception","year":"2010"}}"#;

    /// The keyId-1 static key the `.cjs` embeds in `CRUSH_WASM_B64`.
    const EMBEDDED_KEY_HEX: &str = "0483c7a82132b8516e3eb4061b82e9c881cc585593a4709001131bff7443eabc1701c1f0d50e23ac02b0b9a5979903dbd7e9055aab5e4a5532132d1d200707f5f2";
    /// The live keyId-2 key parsed from `api.wing.st/crush.wasm`'s data
    /// segment (2026-09-25).
    const LIVE_KEY_HEX: &str = "045c88a0ae33c683a4872590b04b22f4774a4fe1c6ecf74785c74a919a71207ca94f9e6b5a7854c3aa3b44ee46bc444fea694b9f23bcd80f864ecbef48c1ef6e14";

    /// The golden nonce for the keyId-1 frame (seed 0x11×32, IV
    /// `AABBCCDDEEFF001122334455`).
    const NONCE_KEY1_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111aabbccddeeff001122334455";
    /// The golden scalar the nonce derives.
    const SCALAR_KEY1_HEX: &str =
        "c7c7eecac57c0a2f0fdef21cfa515f9d836e7f75612bfb4d0ab0281f1ccfda38";
    /// The golden ephemeral public key.
    const EPH_PUB_KEY1_HEX: &str = "04e6e06081d00aee0f4f604af6e41617bc2389b149656a7912fa4278f1c23db38523b22760a4ef4aff67ad5fbfff1da3a6672663b8c7cd260b38e3a53d44ab6316";
    /// The golden s2c key.
    const S2C_KEY1_HEX: &str = "4bb816ff62428c98fc092e0437ef91e7b3e1183a084d7996bc83cadce820892a";
    /// The golden POST body (180 bytes) — wasm-verified byte-for-byte.
    const BODY_KEY1_HEX: &str = "020104e6e06081d00aee0f4f604af6e41617bc2389b149656a7912fa4278f1c23db38523b22760a4ef4aff67ad5fbfff1da3a6672663b8c7cd260b38e3a53d44ab6316aabbccddeeff00112233445575c91f60dbdda841c8b3e62efda7e517084371f4ad58d511f2fd3532c09fb2e15cc549c0d8c78c90d2e982dafc18e22e9ad9bdfbe22be5a51c9f6bc652c6d35c1f51ff67a0399114bec2d75e1c8100782b5a6ff2a1b600cbd87a0eebd90f8daacfc3f2a121";

    /// The golden nonce of the live keyId-2 frame.
    const NONCE_LIVE_HEX: &str =
        "ba0694b47c072dc678977bb54531331f6b0531412cd9684f8172511c14c2ba1092781975efc712e98e7ecb18";
    /// The live frame's full wasm output (278 bytes: s2c ‖ keyId ‖
    /// ephPub ‖ body) — captured from the live module's own
    /// `seal_request`.
    const OUT_LIVE_HEX: &str = "b9cf4f1ad2801296fbbbb060f4d16b535833e84ecd2f532b54a43e95b1653ac9020446d075961d6634b1d5abeddbfd66743190215239028896e9d8d09d077a357828d87502231a59e92f4fc19062d02e8d7e1d09985c8652dbc29d31cffb8baaa6b602020446d075961d6634b1d5abeddbfd66743190215239028896e9d8d09d077a357828d87502231a59e92f4fc19062d02e8d7e1d09985c8652dbc29d31cffb8baaa6b692781975efc712e98e7ecb18904aff543ff3d1c25decc973b0f6a61639e7cc3ccfa8e37d64a2330f7f1b1c3cefd966470b8a2480fe8afb5d04a59c6f84b21550cae626d79ea16fe8f6b69cb86494dbe3b892ff1f05517b945018b528f01159ad675d16087bca787485106da0c7412827df";
    /// The live `/g` answer (189 bytes: IV ‖ ct ‖ tag).
    const RESPONSE_LIVE_HEX: &str = "a094c389ebf98d74fa78713268e444036983b1ed56d5547f0a218dac430d7bac30b2188f2615e82bc7da2ba1bb353b927195f23fd72ab8e947e8f10082ef1e972b881072452a78470ca9c0326943ce4e77b949056f183000cae352d3beb4bf505fe68a524372328fec8a585e51c2c913f98a9f80c41cc6eebf0084c4979c1669c8184eb389e26279aaa43e43957e1f9c053af5897406f859ecca266c1f54d713ae1e9ee2a3045cad3c4319893878469b3a828894e2746f8333b44cc373";

    // ------------------------------------------------------------------
    // Golden frames — the crypto, byte-for-byte.
    // ------------------------------------------------------------------

    #[test]
    fn seal_request_matches_the_embedded_key_golden_frame() {
        // Captured from the wasm's own seal_request under the keyId-1
        // key the .cjs embeds (verified body/header-identical against
        // the live module instance).
        let sealed = seal_request(PAYLOAD, &unhex(NONCE_KEY1_HEX), 1, &unhex(EMBEDDED_KEY_HEX))
            .unwrap_or_else(|| panic!("seal_request failed"));
        assert_eq!(sealed.key_id, 1);
        assert_eq!(sealed.ephemeral_public, unhex(EPH_PUB_KEY1_HEX)[..]);
        assert_eq!(sealed.response_key, unhex(S2C_KEY1_HEX)[..]);
        assert_eq!(sealed.body, unhex(BODY_KEY1_HEX));
    }

    #[test]
    fn seal_request_matches_the_live_rotated_key_frame() {
        // The frame the live endpoint answered 200 for (keyId-2, the
        // rotated static key parsed from the fresh wasm download).
        let sealed = seal_request(PAYLOAD, &unhex(NONCE_LIVE_HEX), 2, &unhex(LIVE_KEY_HEX))
            .unwrap_or_else(|| panic!("seal_request failed"));
        let expected = unhex(OUT_LIVE_HEX);
        let header = &expected[..98];
        assert_eq!(sealed.key_id, header[32]);
        assert_eq!(sealed.ephemeral_public, header[33..98]);
        assert_eq!(sealed.response_key, header[..32]);
        assert_eq!(sealed.body, expected[98..]);
    }

    #[test]
    fn decrypt_response_opens_the_live_frame() {
        let sealed = seal_request(PAYLOAD, &unhex(NONCE_LIVE_HEX), 2, &unhex(LIVE_KEY_HEX))
            .unwrap_or_else(|| panic!("seal_request failed"));
        let reply = decrypt_response(&unhex(RESPONSE_LIVE_HEX), &sealed)
            .unwrap_or_else(|| panic!("decryption failed"));
        assert_eq!(reply.pointer("/status"), Some(&json!(200)));
        assert_eq!(
            reply.pointer("/data/stream/0/playlist"),
            Some(&json!(
                "https://ok.solarpanelcleaning.cc/playlist/PXDm-yFnQdxWAmmXj6JIew.m3u8"
            ))
        );
        assert_eq!(reply.pointer("/data/stream/0/type"), Some(&json!("hls")));
    }

    #[test]
    fn decrypt_response_rejects_tampered_frames() {
        let sealed = seal_request(PAYLOAD, &unhex(NONCE_LIVE_HEX), 2, &unhex(LIVE_KEY_HEX))
            .unwrap_or_else(|| panic!("seal_request failed"));
        let mut tampered = unhex(RESPONSE_LIVE_HEX);
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(decrypt_response(&tampered, &sealed).is_none());
        // And anything shorter than IV + tag.
        assert!(decrypt_response(&unhex("aabbcc"), &sealed).is_none());
    }

    #[test]
    fn ephemeral_scalar_matches_the_golden_hash() {
        let scalar = derive_ephemeral_scalar(&unhex(NONCE_KEY1_HEX))
            .unwrap_or_else(|| panic!("no scalar derived"));
        assert_eq!(scalar.to_bytes_be(), unhex(SCALAR_KEY1_HEX));
    }

    #[test]
    fn both_server_keys_decode_on_curve() {
        for key in [EMBEDDED_KEY_HEX, LIVE_KEY_HEX] {
            let decoded = p256::decode_point(&unhex(key));
            assert!(decoded.is_some(), "key {key} must be a valid P-256 point");
        }
        // Wrong length and wrong prefix are rejected.
        assert!(p256::decode_point(&unhex("0400")).is_none());
        assert!(p256::decode_point(&[0u8; 65]).is_none());
    }

    #[test]
    fn seal_request_validates_its_inputs() {
        // Nonce and key lengths are hard requirements.
        assert!(seal_request(PAYLOAD, &unhex("aabb"), 1, &unhex(EMBEDDED_KEY_HEX)).is_none());
        assert!(seal_request(PAYLOAD, &unhex(NONCE_KEY1_HEX), 1, &unhex("04010203")).is_none());
    }

    // ------------------------------------------------------------------
    // Request building + entry parsing.
    // ------------------------------------------------------------------

    fn movie_info() -> RequestInfo {
        RequestInfo {
            tmdb_id: "27205".to_string(),
            is_tv: false,
            season: None,
            episode: None,
            title: "Inception".to_string(),
            year: Some("2010".to_string()),
        }
    }

    #[test]
    fn build_request_routes_movie_and_series_payloads() {
        let movie = build_request("Lisbon", &movie_info())
            .unwrap_or_else(|| panic!("build_request failed"));
        let parsed: Value = serde_json::from_str(&movie).unwrap_or_else(|_| panic!("bad json"));
        assert_eq!(
            parsed,
            json!({"path": "/Lisbon/movie", "payload": {"tmdb": "27205", "title": "Inception", "year": "2010"}})
        );

        let mut series = movie_info();
        series.is_tv = true;
        series.season = Some(1);
        series.episode = Some(2);
        series.year = None;
        series.title = String::new();
        let series =
            build_request("Athens", &series).unwrap_or_else(|| panic!("build_request failed"));
        let parsed: Value = serde_json::from_str(&series).unwrap_or_else(|_| panic!("bad json"));
        assert_eq!(
            parsed,
            json!({"path": "/Athens/series", "payload": {"tmdb": "27205", "season": "1", "episode": "2"}})
        );
    }

    #[test]
    fn parse_stream_entry_maps_hls_and_file_entries() {
        let info = movie_info();
        let hls: Value = serde_json::from_str(
            r#"{"type":"hls","id":"primary","playlist":"https://cdn.example/x.m3u8","captions":[]}"#,
        )
        .unwrap_or_else(|_| panic!("bad json"));
        let streams = parse_stream_entry(&hls, "Lisbon", &info);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url, "https://cdn.example/x.m3u8");
        assert_eq!(streams[0].quality.as_deref(), Some("2160p"));
        assert!(!streams[0].is_iframe);
        assert_eq!(streams[0].name, "CineJoy - Lisbon (primary)");

        // Non-Lisbon servers answer the 1080p tier.
        let streams = parse_stream_entry(&hls, "Nebula", &info);
        assert_eq!(streams[0].quality.as_deref(), Some("1080p"));

        let file: Value = serde_json::from_str(
            r#"{"type":"file","id":"f1","qualities":{"1080":{"type":"mp4","url":"https://cdn.example/a.mp4"},"unknown":{"type":"m3u8","url":"https://cdn.example/b.m3u8"},"720":{"url":null}}}"#,
        )
        .unwrap_or_else(|_| panic!("bad json"));
        let streams = parse_stream_entry(&file, "Solara", &info);
        assert_eq!(streams.len(), 2);
        let qualities: Vec<&str> = streams
            .iter()
            .filter_map(|s| s.quality.as_deref())
            .collect();
        assert!(qualities.contains(&"1080p"));
        assert!(qualities.contains(&"720p"));
        assert!(
            streams
                .iter()
                .all(|s| s.name.starts_with("CineJoy - Solara "))
        );

        // Unknown entry shapes produce nothing.
        let empty: Value =
            serde_json::from_str(r#"{"type":"other"}"#).unwrap_or_else(|_| panic!("bad json"));
        assert!(parse_stream_entry(&empty, "Lisbon", &info).is_empty());
    }

    #[test]
    fn parse_stream_entry_turns_internal_ids_into_iframes() {
        let mut info = movie_info();
        info.is_tv = true;
        info.season = Some(2);
        info.episode = Some(3);
        let hls: Value =
            serde_json::from_str(r#"{"type":"hls","id":"sub","playlist":"sub","captions":[]}"#)
                .unwrap_or_else(|_| panic!("bad json"));
        let streams = parse_stream_entry(&hls, "Sakura", &info);
        assert_eq!(streams.len(), 1);
        assert!(streams[0].is_iframe);
        assert_eq!(streams[0].url, "https://cinejoy.pk/e/tv/27205/2/3");
        assert_eq!(streams[0].quality.as_deref(), Some("1080p"));

        let file: Value = serde_json::from_str(
            r#"{"type":"file","id":"f","qualities":{"720":{"type":"mp4","url":"sventank-720p"}}}"#,
        )
        .unwrap_or_else(|_| panic!("bad json"));
        let streams = parse_stream_entry(&file, "Joy", &info);
        assert_eq!(streams.len(), 1);
        assert!(streams[0].is_iframe);
        assert_eq!(streams[0].quality.as_deref(), Some("720p"));
    }

    #[test]
    fn server_of_name_extracts_the_scrapers_shape() {
        assert_eq!(server_of_name("CineJoy - Lisbon 1080p (primary)"), "Lisbon");
        assert_eq!(server_of_name("CineJoy - Solara 720 (f)"), "Solara");
        assert_eq!(server_of_name("CineJoy"), "CineJoy");
    }

    // ------------------------------------------------------------------
    // The wrapper conversion.
    // ------------------------------------------------------------------

    #[test]
    fn unwrap_pengu_proxy_decodes_the_resource_frame() {
        // The frame is base64url of {"url":…,"headers":{"Referer":…}}.
        let payload = json!({"url": "https://cdn.example/real.m3u8", "headers": {"Referer": "https://cinejoy.pk/"}});
        let encoded = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let url = format!("https://pengu.uk/hls/cinejoy/resource/{encoded}/media.m3u8");
        let (raw, referer) = unwrap_pengu_proxy(&url);
        assert_eq!(raw, "https://cdn.example/real.m3u8");
        assert_eq!(referer, "https://cinejoy.pk/");
    }

    #[test]
    fn unwrap_pengu_proxy_falls_back_gracefully() {
        let (raw, referer) = unwrap_pengu_proxy("https://cdn.example/plain.m3u8");
        assert_eq!(raw, "https://cdn.example/plain.m3u8");
        assert_eq!(referer, "https://cinejoy.pk/");

        // Undecodable resource frames keep the proxy URL and the
        // default referer.
        let (raw, referer) =
            unwrap_pengu_proxy("https://pengu.uk/hls/cinejoy/resource/!!!not-base64!!!/media.m3u8");
        assert!(raw.contains("pengu.uk/"));
        assert_eq!(referer, "https://cinejoy.pk/");

        // A decoded-but-empty JSON url falls back to the frame URL.
        let payload = json!({"url": ""});
        let encoded = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let url = format!("https://pengu.uk/hls/cinejoy/resource/{encoded}/media.m3u8");
        let (raw, _) = unwrap_pengu_proxy(&url);
        assert_eq!(raw, url);
    }

    fn raw_streams() -> Vec<RawStream> {
        vec![
            RawStream {
                url: "https://cdn.example/lisbon.m3u8".to_string(),
                name: "CineJoy - Lisbon (primary)".to_string(),
                quality: Some("2160p".to_string()),
                is_iframe: false,
            },
            RawStream {
                // Same URL through a pengu proxy — dedups onto the first.
                url: {
                    let payload = json!({"url": "https://cdn.example/lisbon.m3u8"});
                    format!(
                        "https://pengu.uk/hls/cinejoy/resource/{}/media.m3u8",
                        URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
                    )
                },
                name: "CineJoy - Lisbon (primary)".to_string(),
                quality: Some("2160p".to_string()),
                is_iframe: false,
            },
            RawStream {
                url: "https://cdn.example/solara-1080.mp4".to_string(),
                name: "CineJoy - Solara 1080 (f1)".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: false,
            },
            RawStream {
                url: "https://cinejoy.pk/e/movie/27205".to_string(),
                name: "CineJoy - Joy (internal)".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: true,
            },
            RawStream {
                // Non-http URLs are dropped by the wrapper.
                url: "magnet:?xt=urn:btih:x".to_string(),
                name: "CineJoy - Athens".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: false,
            },
        ]
    }

    #[test]
    fn convert_streams_marks_dedups_and_headers() {
        let converted = convert_streams(&raw_streams(), "Inception (2010)");
        // The duplicate pengu frame and the iframe/non-http entries are gone.
        assert_eq!(converted.len(), 2);

        let lisbon = &converted[0];
        assert_eq!(lisbon.url, "https://cdn.example/lisbon.m3u8");
        assert_eq!(lisbon.quality.as_deref(), Some("2160p"));
        assert_eq!(
            lisbon.title.as_deref(),
            Some("Inception (2010) [Lisbon] 2160p WEB-DL HEVC HDR English")
        );
        assert_eq!(lisbon.name.as_deref(), Some("CineJoy - Lisbon"));
        assert_eq!(
            lisbon.headers.get("Referer").map(String::as_str),
            Some("https://cinejoy.pk/")
        );

        let solara = &converted[1];
        assert_eq!(
            solara.title.as_deref(),
            Some("Inception (2010) [Solara] 1080p WEB-DL x264 English")
        );
        assert_eq!(solara.name.as_deref(), Some("CineJoy - Solara"));
    }

    #[test]
    fn converted_streams_build_final_stream_results() {
        let converted = convert_streams(&raw_streams(), "Inception (2010)");
        let streams = build_stream_results(&BuildParams {
            streams: &converted,
            title: "Inception (2010)",
            source_id: ID,
            source_label: LABEL,
            country_codes: &COUNTRY_CODES,
            ttl: TTL,
        });
        assert_eq!(streams.len(), 2);

        let lisbon = &streams[0];
        assert_eq!(lisbon.url.as_str(), "https://cdn.example/lisbon.m3u8");
        assert_eq!(lisbon.format, vsources_core::types::Format::Hls);
        assert_eq!(lisbon.meta.resolution, Some(2160));
        assert_eq!(lisbon.ttl, TTL);
        assert_eq!(
            lisbon
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://cinejoy.pk/")
        );
        assert_eq!(lisbon.meta.source_id.as_deref(), Some("cinejoyaio"));
        assert!(
            lisbon
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Lisbon"))
        );

        let solara = &streams[1];
        assert_eq!(solara.url.as_str(), "https://cdn.example/solara-1080.mp4");
        assert_eq!(solara.format, vsources_core::types::Format::Mp4);
        assert_eq!(solara.meta.resolution, Some(1080));
    }

    // ------------------------------------------------------------------
    // The scripted fetcher (host+path keyed, requests recorded).
    // ------------------------------------------------------------------

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        status: u16,
        body: String,
        content_type: &'static str,
    }

    impl Scripted {
        /// A 200 JSON body.
        fn json(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                body: body.into(),
                content_type: "application/json",
            }
        }

        /// A 200 text body.
        fn text(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                body: body.into(),
                content_type: "text/plain",
            }
        }
    }

    /// A fetcher serving scripted bodies by host+path (in order, the
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

        /// Serve `host + path` with a response.
        fn serve(self, host_path: impl Into<String>, response: Scripted) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(host_path.into())
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
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
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
            let response = match pages.get_mut(&key) {
                Some(queue) if !queue.is_empty() => {
                    let front = queue.front().cloned();
                    if queue.len() > 1 {
                        queue.pop_front();
                    }
                    front
                }
                _ => None,
            };
            match response {
                Some(scripted) if (200..300).contains(&scripted.status) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        scripted.content_type.to_string(),
                    )]),
                    body: scripted.body,
                }),
                Some(scripted) => Err(FetchError::Http {
                    url: request.url,
                    status: scripted.status,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A resolve context over the mock.
    fn ctx(fetcher: &MockFetcher) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media: None,
            source_id: None,
            referer: None,
        }
    }

    /// A provider whose TMDB client shares the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> CineJoyAllInOne {
        CineJoyAllInOne::new(Arc::new(TmdbClient::new("k", fetcher.clone())))
    }

    #[tokio::test]
    async fn roster_fetch_filters_ok_servers_and_caches() {
        let fetcher = MockFetcher::new().serve(
            "api.wing.st/servers",
            Scripted::json(
                r#"{"servers":[{"name":"Lisbon","status":"ok"},{"name":"Joy","status":"down"},{"name":"Athens","status":"ok"},{"status":"ok"},{"name":"","status":"ok"}]}"#,
            ),
        );
        let ctx = ctx(&fetcher);
        let provider = provider(&Arc::new(MockFetcher::new()));
        let servers = provider.roster(&ctx).await;
        assert_eq!(servers, vec!["Lisbon".to_string(), "Athens".to_string()]);

        // The 30-minute cache holds: a second fetch within the TTL does
        // not hit the endpoint again.
        let again = provider.roster(&ctx).await;
        assert_eq!(again, servers);
        assert_eq!(
            fetcher
                .requests()
                .iter()
                .filter(|request| request.url.path() == "/servers")
                .count(),
            1
        );

        // The roster request carries the site headers.
        let roster_request = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/servers")
            .cloned()
            .unwrap_or_else(|| panic!("no /servers request"));
        assert_eq!(
            roster_request.headers.get("Origin").map(String::as_str),
            Some("https://cinejoy.pk")
        );
        assert_eq!(
            roster_request.headers.get("Referer").map(String::as_str),
            Some("https://cinejoy.pk/")
        );
        assert!(roster_request.headers.contains_key("User-Agent"));
    }

    #[tokio::test]
    async fn roster_falls_back_when_the_endpoint_fails() {
        // Nothing scripted: every request 404s.
        let fetcher = MockFetcher::new();
        let plain_ctx = ctx(&fetcher);
        let plain_provider = provider(&Arc::new(MockFetcher::new()));
        let servers = plain_provider.roster(&plain_ctx).await;
        assert_eq!(
            servers,
            FALLBACK_SERVERS
                .iter()
                .copied()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        // An empty ok-roster also falls back.
        let fetcher =
            MockFetcher::new().serve("api.wing.st/servers", Scripted::json(r#"{"servers":[]}"#));
        let empty_ctx = ctx(&fetcher);
        let empty_provider = provider(&Arc::new(MockFetcher::new()));
        assert_eq!(
            empty_provider.roster(&empty_ctx).await,
            FALLBACK_SERVERS
                .iter()
                .copied()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn validate_stream_url_drops_broken_synthetic_urls() {
        let fetcher = MockFetcher::new()
            .serve(
                "cdn.example/synthetic/ok/playlist.m3u8",
                Scripted::text("#EXTM3U\n#EXT-X-VERSION:3\n"),
            )
            .serve(
                "cdn.example/synthetic/expired/playlist.m3u8",
                Scripted {
                    status: 404,
                    body: "Expired".to_string(),
                    content_type: "text/plain",
                },
            )
            .serve(
                "cdn.example/synthetic/empty/playlist.m3u8",
                Scripted::text(""),
            );
        let ctx = ctx(&fetcher);
        assert!(validate_stream_url(&ctx, "https://cdn.example/synthetic/ok/playlist.m3u8").await);
        assert!(
            !validate_stream_url(&ctx, "https://cdn.example/synthetic/expired/playlist.m3u8").await
        );
        assert!(
            !validate_stream_url(&ctx, "https://cdn.example/synthetic/empty/playlist.m3u8").await
        );

        // The probe sends the Range header.
        let probe = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path().starts_with("/synthetic/ok"))
            .cloned()
            .unwrap_or_else(|| panic!("no probe request"));
        assert_eq!(
            probe.headers.get("Range").map(String::as_str),
            Some("bytes=0-3")
        );
    }

    #[tokio::test]
    async fn sweep_drops_broken_synthetic_streams() {
        let fetcher = MockFetcher::new().serve(
            "cdn.example/synthetic/ok/playlist.m3u8",
            Scripted::text("#EXTM3U\n"),
        );
        let ctx = ctx(&fetcher);
        let mut streams = vec![
            RawStream {
                url: "https://cdn.example/plain.m3u8".to_string(),
                name: "CineJoy - Lisbon".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: false,
            },
            RawStream {
                url: "https://cdn.example/synthetic/ok/playlist.m3u8".to_string(),
                name: "CineJoy - Athens".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: false,
            },
            RawStream {
                // Nothing scripted for this host → 404 → dropped.
                url: "https://cdn.example/synthetic/broken/playlist.m3u8".to_string(),
                name: "CineJoy - Castle".to_string(),
                quality: Some("1080p".to_string()),
                is_iframe: false,
            },
        ];
        validate_synthetic(&ctx, &mut streams).await;
        let urls: Vec<&str> = streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://cdn.example/plain.m3u8",
                "https://cdn.example/synthetic/ok/playlist.m3u8",
            ]
        );
    }

    // ------------------------------------------------------------------
    // End-to-end resolves over the scripted fetcher.
    // ------------------------------------------------------------------

    /// A mock serving the TMDB movie details and the roster.
    fn movie_stack() -> Arc<MockFetcher> {
        Arc::new(
            MockFetcher::new()
                .serve(
                    "api.themoviedb.org/3/movie/27205",
                    Scripted::json(
                        r#"{"id":27205,"title":"Inception","release_date":"2010-07-16","original_title":"Inception"}"#,
                    ),
                )
                .serve(
                    "api.wing.st/servers",
                    Scripted::json(
                        r#"{"servers":[{"name":"Lisbon","status":"ok"},{"name":"Nebula","status":"ok"},{"name":"Athens","status":"ok"}]}"#,
                    ),
                ),
        )
    }

    #[tokio::test]
    async fn resolve_returns_empty_when_the_sealed_transport_is_unavailable() {
        // The transport cut: the roster is fetched, every server sweep
        // fails at the sealed POST, and the provider degrades to the
        // upstream's empty-on-server-failure answer.
        let fetcher = movie_stack();
        let provider = provider(&fetcher);
        let ctx = ctx(&fetcher);
        let media = MediaRef {
            id: MediaId::Tmdb(27205),
            kind: MediaType::Movie,
            season: None,
            episode: None,
        };
        let streams = provider.resolve(&ctx, &media).await;
        assert!(
            matches!(&streams, Ok(result) if result.is_empty()),
            "expected an empty success, got {streams:?}"
        );

        let requests = fetcher.requests();
        // TMDB details + the roster were consulted (the roster exactly
        // once — the 30-minute cache holds across the empty-retry).
        assert!(
            requests
                .iter()
                .any(|request| request.url.path() == "/3/movie/27205")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/servers")
                .count(),
            1
        );
        // The sealed POST never left: the binary frame has no
        // String-transport path.
        assert!(!requests.iter().any(|request| {
            request.url.host_str() == Some("api.wing.st") && request.url.path() == "/g"
        }));
    }

    #[tokio::test]
    async fn resolve_maps_series_without_episode_to_empty() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "api.themoviedb.org/3/tv/1396",
                    Scripted::json(
                        r#"{"id":1396,"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
                    ),
                )
                .serve(
                    "api.wing.st/servers",
                    Scripted::json(r#"{"servers":[{"name":"Lisbon","status":"ok"}]}"#),
                ),
        );
        let provider = provider(&fetcher);
        let ctx = ctx(&fetcher);
        let media = MediaRef {
            id: MediaId::Tmdb(1396),
            kind: MediaType::Series,
            season: Some(1),
            episode: None,
        };
        let streams = provider.resolve(&ctx, &media).await;
        assert!(
            matches!(&streams, Ok(result) if result.is_empty()),
            "expected an empty success, got {streams:?}"
        );
        // The .cjs guard fires before any roster fetch.
        assert!(
            !fetcher
                .requests()
                .iter()
                .any(|request| request.url.path() == "/servers")
        );
    }

    #[tokio::test]
    async fn resolve_propagates_tmdb_not_found() {
        // The IMDb mapping answers no results → NotFound.
        let fetcher = Arc::new(MockFetcher::new().serve(
            "api.themoviedb.org/3/find/tt0000000",
            Scripted::json(r#"{"movie_results":[],"tv_results":[]}"#),
        ));
        let provider = provider(&fetcher);
        let ctx = ctx(&fetcher);
        let media = MediaRef {
            id: MediaId::Imdb("tt0000000".to_string()),
            kind: MediaType::Movie,
            season: None,
            episode: None,
        };
        assert!(matches!(
            provider.resolve(&ctx, &media).await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn resolve_uses_pre_resolved_media_metadata() {
        // ctx.media short-circuits the TMDB details fetch (the wrapper's
        // getTmdbNameAndYear cache behavior).
        let fetcher = Arc::new(MockFetcher::new().serve(
            "api.wing.st/servers",
            Scripted::json(r#"{"servers":[{"name":"Lisbon","status":"ok"}]}"#),
        ));
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(vsources_core::traits::ResolvedMedia {
                tmdb_id: Some(27205),
                imdb_id: None,
                name: "Inception".to_string(),
                year: Some(2010),
                season: None,
                episode: None,
            }),
            source_id: None,
            referer: None,
        };
        let provider = provider(&fetcher);
        let media = MediaRef {
            id: MediaId::Tmdb(27205),
            kind: MediaType::Movie,
            season: None,
            episode: None,
        };
        assert!(
            provider
                .resolve(&ctx, &media)
                .await
                .unwrap_or_default()
                .is_empty()
        );
        assert!(
            !fetcher
                .requests()
                .iter()
                .any(|request| request.url.host_str() == Some("api.themoviedb.org"))
        );
    }
    #[tokio::test]
    async fn sealed_transport_posts_binary_and_decrypts_the_golden_response() {
        struct BinaryFetcher {
            expected: Vec<u8>,
            reply: Vec<u8>,
        }
        #[async_trait]
        impl Fetcher for BinaryFetcher {
            async fn request(&self, _: FetchRequest) -> Result<FetchResponse, FetchError> {
                panic!("binary API must not use text")
            }
            async fn probe(
                &self,
                request: FetchRequest,
                limit: usize,
            ) -> Result<Option<vsources_core::traits::ProbeResponse>, FetchError> {
                assert_eq!(request.method, "POST");
                assert_eq!(request.url.path(), "/g");
                assert_eq!(
                    request.binary_body.as_deref(),
                    Some(self.expected.as_slice())
                );
                assert_eq!(
                    request.headers.get("Content-Type").map(String::as_str),
                    Some("application/octet-stream")
                );
                assert!(self.reply.len() < limit);
                Ok(Some(vsources_core::traits::ProbeResponse {
                    url: request.url,
                    status: 200,
                    headers: std::collections::BTreeMap::default(),
                    body: self.reply.clone(),
                    truncated: false,
                }))
            }
        }
        let sealed = seal_request(PAYLOAD, &unhex(NONCE_LIVE_HEX), 2, &unhex(LIVE_KEY_HEX))
            .unwrap_or_else(|| panic!("golden seal"));
        let fetcher = BinaryFetcher {
            expected: sealed.body.clone(),
            reply: unhex(RESPONSE_LIVE_HEX),
        };
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: None,
        };
        let response = sealed_post(&ctx, &sealed)
            .await
            .unwrap_or_else(|| panic!("binary response"));
        assert_eq!(
            decrypt_response(&response, &sealed).and_then(|v| v.get("status").cloned()),
            Some(json!(200))
        );
    }
}
