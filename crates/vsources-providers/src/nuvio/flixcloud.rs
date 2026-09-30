//! `FlixCloud` decryption chain — the port of the WASM-driven part of
//! `src/nuvio/reanime.cjs` (`resolveFlixcloud`).
//!
//! flixcloud.cc embeds a tiny (406-byte) WebAssembly module in every
//! `/e/{accessId}?v=2` page as base64 (`w_payload`). The module
//! exports `_s`, `_r`, and `_c`, and the JS feeds it three same-length
//! key fragments plus a seed-derived index to produce the AES key
//! fragment `O`. This port **parses the module instead of executing
//! it** (the workspace has no WASM runtime), which the disassembly of
//! the live module justifies:
//!
//! - `_s(v)` stores `v` (the seed's first 8 hex chars as u32) in a
//!   mutable global;
//! - `_r(t, e, a, out, k)` mixes each byte: `x = t[i] ^ e[i] ^ a[i]`
//!   through an arithmetic chain (see [`mix_key`]) with
//!   `(v + i·36) & 255` folded in;
//! - `_c()` XORs the two 32-byte halves of the module's 64-byte data
//!   segment and returns the offset — i.e. the per-session m3u8 XOR
//!   key is embedded in the module's data section.
//!
//! Both behaviors are reimplemented in pure Rust and verified
//! byte-for-byte against a live capture (seed, fragments, token, IV,
//! and ciphertext from a real `/e/` page) in the tests below.
//!
//! The remaining chain (PBKDF2 → XOR-seed → SHA-256 → AES-256-CBC) is
//! shared with [`crate::nuvio::decrypt::derive_aes_key_chain`].

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::decrypt::{aes256_cbc_decrypt, derive_aes_key_chain, derive_field_names, sha256_hex};
use serde_json::Value;

/// The PBKDF2 iteration count the JS hardcodes.
const PBKDF2_ITERATIONS: u32 = 1000;

/// The raw material extracted from a flixcloud `/e/` page and its
/// `/api/m3u8/{token}` response.
#[derive(Debug, Clone)]
pub struct FlixMaterials {
    /// The page's `obfuscation_seed`.
    pub seed: String,
    /// The first key fragment (`kf_…` field, base64).
    pub frag1_b64: String,
    /// The IV (`ivf_…` field, base64).
    pub iv_b64: String,
    /// The m3u8 key fragment (`/api/m3u8`'s `key` entry, base64).
    pub key_b64: String,
    /// The second key fragment (`hash_a`-derived field, base64).
    pub key_frag2_b64: String,
    /// The encrypted master URL (`/api/m3u8`'s `vid` entry, base64).
    pub vid_b64: String,
    /// The base64 `w_payload` WASM module.
    pub w_payload_b64: String,
}

/// The resolved `FlixCloud` stream.
#[derive(Debug, Clone)]
pub struct FlixStream {
    /// The decrypted master `.m3u8` URL.
    pub master_url: String,
    /// The per-session 32-byte XOR key (`_c`'s output — `window.__pk`
    /// in the browser), base64-encoded for stream metadata.
    pub xor_key_b64: String,
}

/// The `_r` byte-mixing function, transcribed from the live module's
/// bytecode (all arithmetic wrapping, u32, truncated to a byte at the
/// store):
///
/// ```text
/// x  = t[i] ^ e[i] ^ a[i]
/// x  = (x - 7) & 255
/// x  = (x >> 3) | ((x << 5) & 255)      // rotate left 5
/// x  = (x + 56) & 255
/// x  = (x - 204) & 255
/// x  = (x - 57) & 255
/// x  = ((x << 2) & 255) | (x >> 6)      // rotate left 2
/// x  = ((x << 5) & 255) | (x >> 3)      // rotate left 5
/// x ^= (v + i·36) & 255
/// O[i] = x as u8
/// ```
#[must_use]
pub fn mix_key(frag1: &[u8], frag2: &[u8], frag3: &[u8], seed: u32) -> Vec<u8> {
    let length = frag1.len().min(frag2.len()).min(frag3.len());
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let mut value = u32::from(frag1[index] ^ frag2[index] ^ frag3[index]);
        value = value.wrapping_sub(7) & 255;
        value = (value >> 3) | ((value << 5) & 255);
        value = value.wrapping_add(56) & 255;
        value = value.wrapping_sub(204) & 255;
        value = value.wrapping_sub(57) & 255;
        value = ((value << 2) & 255) | (value >> 6);
        value = ((value << 5) & 255) | (value >> 3);
        let idx = seed.wrapping_add(u32::try_from(index).unwrap_or(u32::MAX) * 36) & 255;
        value ^= idx;
        out.push(u8::try_from(value & 255).unwrap_or(0));
    }
    out
}

/// The `_c()` output: the XOR of the two 32-byte halves of the module's
/// data segment (the per-session `__pk` key).
///
/// The parser walks the WASM section headers to the data section
/// (id 11) and takes the first segment of ≥ 64 bytes — the live module
/// ships exactly one 64-byte segment at offset 2000, which `_c` mixes
/// in place.
#[must_use]
pub fn parse_wasm_xor_key(wasm: &[u8]) -> Option<[u8; 32]> {
    let data = data_section_bytes(wasm)?;
    if data.len() < 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = data[index] ^ data[index + 32];
    }
    Some(key)
}

/// The bytes of the first data segment of a WASM module — a minimal
/// section walk (magic, version, then id/size/payload sections).
fn data_section_bytes(wasm: &[u8]) -> Option<&[u8]> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return None;
    }
    let mut offset = 8;
    while offset < wasm.len() {
        let section_id = *wasm.get(offset)?;
        let (size, consumed) = read_leb_u32(wasm.get(offset + 1..)?)?;
        offset += 1 + consumed;
        let end = offset.checked_add(size as usize)?;
        let payload = wasm.get(offset..end)?;
        if section_id == 11 {
            // count (leb) then segments: flags (leb), offset expr
            // (`0x41 … 0x0b`), vec(bytes) = leb len + data.
            let (count, consumed) = read_leb_u32(payload)?;
            let mut cursor = consumed;
            for _ in 0..count {
                let (_flags, flag_len) = read_leb_u32(payload.get(cursor..)?)?;
                cursor += flag_len;
                // The offset expression: i32.const … end.
                if *payload.get(cursor)? != 0x41 {
                    return None;
                }
                cursor += 1;
                while payload.get(cursor).is_some_and(|byte| *byte != 0x0b) {
                    cursor += 1;
                }
                cursor += 1;
                let (byte_len, len_size) = read_leb_u32(payload.get(cursor..)?)?;
                cursor += len_size;
                let segment = payload.get(cursor..cursor + byte_len as usize)?;
                if segment.len() >= 64 {
                    return Some(segment);
                }
                cursor += byte_len as usize;
            }
            return None;
        }
        offset = end;
    }
    None
}

/// Read a LEB128 u32, returning (value, bytes consumed).
fn read_leb_u32(bytes: &[u8]) -> Option<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift = 0;
    for (index, byte) in bytes.iter().enumerate() {
        let payload = u32::from(byte & 0x7f);
        result = result.checked_add(payload.checked_shl(shift)?)?;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, index + 1));
        }
        if shift > 28 {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Page + `/api/m3u8` scraping — the regex half of `resolveFlixcloud`.
// ---------------------------------------------------------------------------

/// The payload scraped from a flixcloud `/e/{accessId}?v=2` page.
///
/// The page embeds a JS object literal with a mix of quoted and
/// unquoted keys; the seed-derived field names hide the crypto
/// fields, and decoy fields with the same `{hex}_{hex}` shape sit
/// nearby, so lookups go by exact derived name.
#[derive(Debug, Clone)]
pub struct FlixPage {
    /// The page's `obfuscation_seed`.
    pub seed: String,
    /// The base64 WASM module (`w_payload`).
    pub w_payload_b64: String,
    /// The `video_title`, when present.
    pub video_title: Option<String>,
    /// The `video_id`, when present.
    pub video_id: Option<String>,
    /// The first key fragment, nested inside `obfuscated_crypto_data`.
    pub frag1_b64: String,
    /// The IV, nested inside `obfuscated_crypto_data`.
    pub iv_b64: String,
    /// The token that addresses `/api/m3u8/{token}`.
    pub token_value: String,
    /// The second key fragment.
    pub key_frag2_b64: String,
}

impl FlixPage {
    /// Combine the page payload with the `/api/m3u8` token fields into
    /// the decrypt chain's input.
    #[must_use]
    pub fn materials(&self, vid_b64: &str, key_b64: &str) -> FlixMaterials {
        FlixMaterials {
            seed: self.seed.clone(),
            frag1_b64: self.frag1_b64.clone(),
            iv_b64: self.iv_b64.clone(),
            key_b64: key_b64.to_string(),
            key_frag2_b64: self.key_frag2_b64.clone(),
            vid_b64: vid_b64.to_string(),
            w_payload_b64: self.w_payload_b64.clone(),
        }
    }
}

/// Scrape the `/e/` page — ports the regex half of `resolveFlixcloud`.
///
/// `None` when the seed, the WASM payload, or any crypto field is
/// missing (the JS throws and the provider skips the server).
#[must_use]
pub fn parse_flix_page(html: &str) -> Option<FlixPage> {
    let seed = string_field(html, "obfuscation_seed")?.to_string();
    let w_payload_b64 = string_field(html, "w_payload")?.to_string();
    let video_title = string_field(html, "video_title").map(str::to_string);
    let video_id = string_field(html, "video_id").map(str::to_string);

    let fields = derive_field_names(&seed);

    // The crypto pair must sit inside the nested
    // `obfuscated_crypto_data` container — search only after it.
    let container = field_position(html, &fields.container_name)?;
    let key_at = field_position_after(html, &fields.key_field, container)?;
    let frag1_b64 = value_at(html, key_at)?.to_string();
    let iv_at = field_position_after(html, &fields.iv_field, key_at)?;
    let iv_b64 = value_at(html, iv_at)?.to_string();

    // The token and the second fragment live elsewhere in the payload,
    // addressed by their exact derived names.
    let token_value = string_field(html, &fields.token_field)?.to_string();
    let key_frag2_b64 = string_field(html, &fields.key_frag2_field)?.to_string();

    Some(FlixPage {
        seed,
        w_payload_b64,
        video_title,
        video_id,
        frag1_b64,
        iv_b64,
        token_value,
        key_frag2_b64,
    })
}

/// The `/api/m3u8/{token}` response pair — the JSON keys are
/// `sha256(token + "vid")` and `sha256(token + "key")` truncated to 10
/// hex chars (`(vid_b64, key_b64)`).
///
/// `None` when either key is absent.
#[must_use]
pub fn m3u8_token_fields(json: &Value, token_value: &str) -> Option<(String, String)> {
    let vid_hash = sha256_hex(&format!("{token_value}vid"));
    let key_hash = sha256_hex(&format!("{token_value}key"));
    let vid = json.get(&vid_hash[..10])?.as_str()?;
    let key = json.get(&key_hash[..10])?.as_str()?;
    Some((vid.to_string(), key.to_string()))
}

/// The offset of `field` used as a whole key anywhere in `html`.
fn field_position(html: &str, field: &str) -> Option<usize> {
    field_position_after(html, field, 0)
}

/// The offset of `field` used as a whole key at or after `from`.
///
/// The page mixes unquoted JS keys (`obfuscation_seed:`) with quoted
/// ones (`"{hex}_{hex}":`); a match must start at an identifier
/// boundary and be followed by an optional quote and `:`.
fn field_position_after(html: &str, field: &str, from: usize) -> Option<usize> {
    let mut cursor = from;
    while let Some(found) = html[cursor..].find(field) {
        let start = cursor + found;
        let after = start + field.len();
        let boundary = html[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let rest = &html[after..];
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        if boundary && rest.starts_with(':') {
            return Some(start);
        }
        cursor = after;
    }
    None
}

/// The quoted string value of the key at `position`.
fn value_at(html: &str, position: usize) -> Option<&str> {
    let colon = html[position..].find(':')? + position;
    let rest = html[colon + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The `"?field"?: "value"` lookup over the whole page.
fn string_field<'a>(html: &'a str, field: &str) -> Option<&'a str> {
    value_at(html, field_position(html, field)?)
}

/// Resolve the full chain: WASM key derivation → PBKDF2 → AES-256-CBC
/// decrypt of `vid_b64` → the master URL, plus the per-session XOR key
/// — ports `resolveFlixcloud`'s crypto half. `None` on any failed
/// step (the JS throws and the provider skips the server).
#[must_use]
pub fn resolve_flix_stream(materials: &FlixMaterials) -> Option<FlixStream> {
    let seed = u32::from_str_radix(materials.seed.get(0..8)?, 16).ok()?;

    // The WASM's data segment holds the per-session XOR key.
    let wasm = STANDARD.decode(&materials.w_payload_b64).ok()?;
    let xor_key = parse_wasm_xor_key(&wasm)?;

    // _r mixes the three fragments into the AES key fragment O.
    let frag1 = STANDARD.decode(&materials.frag1_b64).ok()?;
    let frag2 = STANDARD.decode(&materials.key_frag2_b64).ok()?;
    let frag3 = STANDARD.decode(&materials.key_b64).ok()?;
    let o = mix_key(&frag1, &frag2, &frag3, seed);

    // PBKDF2 → XOR seed → SHA-256 → the AES key; decrypt the master URL.
    let key = derive_aes_key_chain(&o, &materials.seed, PBKDF2_ITERATIONS);
    let iv = STANDARD.decode(&materials.iv_b64).ok()?;
    let ciphertext = STANDARD.decode(&materials.vid_b64).ok()?;
    let plaintext = aes256_cbc_decrypt(&key, &iv, &ciphertext)?;
    let master_url = String::from_utf8(plaintext).ok()?;

    Some(FlixStream {
        master_url,
        xor_key_b64: STANDARD.encode(xor_key),
    })
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    /// The live-captured `/e/vimu1sw5xonj?v=2` material (2026-09):
    /// every field below came off the real page, and the WASM is the
    /// real 406-byte module.
    fn live_materials() -> FlixMaterials {
        FlixMaterials {
            seed: "d231526af61cb187".to_string(),
            frag1_b64: "DSxOQu715GZX/ZY8t5r4DuTisXgzOCBpRMlis2gp814=".to_string(),
            iv_b64: "XOw1a12lw6Iui3lDmaAU+Q==".to_string(),
            key_b64: "9xsi+5vEQVghIGHFCPf3z4Cmlp6VtbVCMT/RFECq6kA=".to_string(),
            key_frag2_b64: "yblBgEX8Lm/8hqiFRMdHhmeHQtHggAJ/zxP+K9qdijw=".to_string(),
            vid_b64: "k0+di6VPrLtFKEQzi/xbLLIyQG0AiTzHsY/fysKCDftlgRkSe1+B8rugfP+0N7TIyQgZLj13IsgsXbaqXkQmVp/owcKVBKwhOCy6rahiKQ6/FvYbKPhNXWuN+Zqb8hFPNoZWiff9xgqBKkICVYV719Estb9livtn2YzYrpcpbIj4dvp1tYF+/GG7A5DmtIDZSKD58w4iL51HB7z0HiLrZSxxdV7d1KOqsOfhCisnhGpDXKcYK62ahcxUE9QkHqdYXBrWXC3Jmp301y6py9mPNfwiJZfvb1BFiuYl4aW7zrKoBcx69P8U6dSYy7uLn050UVgF9f4mr4HAdWhxQOBob9bXiHYlHNXzDwFeyvT8mFxGKfC/84FmKkjNdM+BTx9IV7SH8F3D9Y2AnJvsZPitxys3YgawFVkECEA3lkxd4hPKEBfT10dSo0YFMavSIsgmgnUAlzpWq3IB84gLAx/oQkDtyL0bC+jBrLh+9dDxYvY=".to_string(),
            w_payload_b64: "AGFzbQEAAAABEQNgAX8AYAV/f39/fwBgAAF/AwQDAAECBQMBAAEGBgF/AUEACwcZBAZtZW1vcnkCAAJfcwAAAl9yAAECX2MAAgqBAgMGACAAJAALuQEBA39BACEFA0ACQCAFIARPDQAgACAFai0AACABIAVqLQAAcyACIAVqLQAAcyEGIAZBB2tB/wFxIQYgBkEDdiAGQQV0Qf8BcXIhBiAGQThqQf8BcSEGIAZBzAFrQf8BcSEGIAZBOWtB/wFxIQYgBkECdEH/AXEgBkEGdnIhBiAGQQV0Qf8BcSAGQQN2ciEGIAVBJGwjAGpB/wFxIQcgBiAHcyEGIAMgBWogBjoAACAFQQFqIQUMAQsLCz0BAX9BACEAA0ACQCAAQSBPDQBBkBAgAGpB0A8gAGotAABB8A8gAGotAABzOgAAIABBAWohAAwBCwtBkBALC0cBAEHQDwtATaKqw/G2AbW7MrfEanX/4pSun/PjKX+FPpjrnJiEjAnzid2QFS8ObKFHqsfejfvbGY5oAsMlhWTpxShaNhG4Zw==".to_string(),
        }
    }

    /// The master URL the JS chain produced for the same material.
    const LIVE_MASTER_URL: &str = "https://fetch8.flixcloud.cc/_v7/e0dfc0c5-7712-4567-9088-d86701425be4/master.m3u8?token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ2aWRlb19pZCI6ImUwZGZjMGM1LTc3MTItNDU2Ny05MDg4LWQ4NjcwMTQyNWJlNCIsImNsaWVudF9pcCI6IjY2LjIzMS40NC4yMjciLCJleHAiOjE3OTAzNTMwNzIsImlhdCI6MTc5MDMzMTQ3MiwiaXNzIjoidmlkZW8taG9zdGluZy1wbGF0Zm9ybSJ9.2AzP02FauIzJIGi3i688EqyNNTFH9qZXc30JoDuUO0c";

    /// The WASM `_r` output for the live fragments (`O` in the JS).
    const LIVE_O_HEX: &str = "361f496ad69ba3585b70cd06f2edcf1bc3ab8d8a37a720c81ee9efc702f4c08d";

    #[test]
    fn parses_the_live_wasm_xor_key() {
        let wasm = STANDARD
            .decode(live_materials().w_payload_b64)
            .unwrap_or_else(|e| panic!("valid wasm base64: {e}"));
        assert_eq!(wasm.len(), 406);
        let key = parse_wasm_xor_key(&wasm);
        // Node's WebAssembly run of the same module: mem[2000..2032] ^
        // mem[2032..2064].
        let expected: [u8; 32] = [
            0xbe, 0x2b, 0x77, 0x53, 0xe4, 0x99, 0x0f, 0xd9, 0x1a, 0x75, 0x1d, 0x03, 0xb4, 0xf8,
            0x04, 0x39, 0x8d, 0x20, 0xf7, 0xf1, 0x20, 0x0c, 0xfa, 0xe1, 0xd7, 0x5d, 0xc3, 0xc6,
            0xae, 0x95, 0x34, 0x6e,
        ];
        assert_eq!(key, Some(expected));
    }

    #[test]
    fn mix_key_matches_the_live_wasm_output() {
        let materials = live_materials();
        let frag1 = STANDARD
            .decode(&materials.frag1_b64)
            .unwrap_or_else(|e| panic!("valid fragment: {e}"));
        let frag2 = STANDARD
            .decode(&materials.key_frag2_b64)
            .unwrap_or_else(|e| panic!("valid fragment: {e}"));
        let frag3 = STANDARD
            .decode(&materials.key_b64)
            .unwrap_or_else(|e| panic!("valid fragment: {e}"));
        let seed = u32::from_str_radix(&materials.seed[..8], 16)
            .unwrap_or_else(|e| panic!("valid seed prefix: {e}"));
        let o = mix_key(&frag1, &frag2, &frag3, seed);
        let expected: Vec<u8> = (0..LIVE_O_HEX.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&LIVE_O_HEX[i..i + 2], 16).unwrap_or(0))
            .collect();
        assert_eq!(o, expected);
    }

    /// The `/e/vimu1sw5xonj?v=2` page (2026-09 live capture), reduced
    /// to the payload region the scraper reads — same field order, same
    /// decoys (`{hex}_{hex}` keys), same mixed quoted/unquoted keys.
    fn live_page() -> String {
        let m = live_materials();
        format!(
            r#"<body>data: [null,null,{{type:"data",data:{{"42446185cbe125a1_cbe125a1":"c9b23d633ce35ebd18031cb0",available_fonts:{{LTFinnegan_MediumIt:1}}}}}}]
			is_iframe:false,obfuscation_seed:"{seed}","57c01216adfdf9c3440859d4698792fb_2608093f":"df95627f881ea096141d836e",default_audio_track:0,is_domain_owner:false,"790430a10008fa831d94dd58_2185c397a1a90e8b":"82ae1ce75577b128e5082e07",obfuscated_crypto_data:{{cd_2e15a574:{{ad_5e624ac7:[{{od_e28700f6:{{kf_da67e9be:"{frag1}",ivf_cb9d11d5:"{iv}",db87:"0.uuivz6wdlik",ed93:"XO0zsibQ17Q=",metadata:{{timestamp:1790331472437,version:"2.1",encoding:"aes256cbc"}}}}]}}}},bb241621763744c558a8d7b1f3f38bb1:"bcb66e66602e3b1861e57039",intro_chapter:{{start:47,end:136,title:"OP"}},aid:"vimu1sw5xonj",video_title:"[Anime Time] Haikyuu!! - 01.mkv","3893eee5f98fb215_d6b7d80f":"{frag2}",video_id:"e0dfc0c5-7712-4567-9088-d86701425be4",w_payload:"{wasm}"</body>"#,
            seed = m.seed,
            frag1 = m.frag1_b64,
            iv = m.iv_b64,
            frag2 = m.key_frag2_b64,
            wasm = m.w_payload_b64,
        )
    }

    /// The `/api/m3u8/c9b23d633ce35ebd18031cb0` response (live capture)
    /// — the keys are the sha256 token derivations the server uses.
    fn live_m3u8_json() -> Value {
        let m = live_materials();
        serde_json::json!({
            "2e20e3f597": m.vid_b64,
            "68141d28e2": m.key_b64,
        })
    }

    #[test]
    fn parses_the_live_page() {
        let page = parse_flix_page(&live_page());
        let page = page.unwrap_or_else(|| panic!("the live page parses"));
        assert_eq!(page.seed, "d231526af61cb187");
        assert_eq!(page.token_value, "c9b23d633ce35ebd18031cb0");
        assert_eq!(
            page.video_id.as_deref(),
            Some("e0dfc0c5-7712-4567-9088-d86701425be4")
        );
        assert_eq!(
            page.video_title.as_deref(),
            Some("[Anime Time] Haikyuu!! - 01.mkv")
        );
        let materials = live_materials();
        assert_eq!(page.frag1_b64, materials.frag1_b64);
        assert_eq!(page.iv_b64, materials.iv_b64);
        assert_eq!(page.key_frag2_b64, materials.key_frag2_b64);
        assert_eq!(page.w_payload_b64, materials.w_payload_b64);
    }

    #[test]
    fn page_without_crypto_container_fails_closed() {
        let html = r#"obfuscation_seed:"d231526af61cb187",w_payload:"AAAA""#;
        assert!(parse_flix_page(html).is_none());
        // Decoy-shaped keys must not satisfy the token lookup.
        let html = r#"obfuscation_seed:"d231526af61cb187",w_payload:"AAAA",cd_2e15a574:{kf_da67e9be:"a",ivf_cb9d11d5:"b"}"#;
        assert!(parse_flix_page(html).is_none());
    }

    #[test]
    fn m3u8_token_fields_use_the_sha256_derivations() {
        let (vid, key) = m3u8_token_fields(&live_m3u8_json(), "c9b23d633ce35ebd18031cb0")
            .unwrap_or_else(|| panic!("the live token response parses"));
        let materials = live_materials();
        assert_eq!(vid, materials.vid_b64);
        assert_eq!(key, materials.key_b64);
        assert!(m3u8_token_fields(&serde_json::json!({}), "c9b23d633ce35ebd18031cb0").is_none());
    }

    #[test]
    fn page_scrape_plus_m3u8_resolves_the_live_master_url() {
        let page = parse_flix_page(&live_page()).unwrap_or_else(|| panic!("the live page parses"));
        let (vid, key) = m3u8_token_fields(&live_m3u8_json(), &page.token_value)
            .unwrap_or_else(|| panic!("the token response parses"));
        let stream = resolve_flix_stream(&page.materials(&vid, &key))
            .unwrap_or_else(|| panic!("the live chain resolves"));
        assert_eq!(stream.master_url, LIVE_MASTER_URL);
    }

    #[test]
    fn resolves_the_live_master_url_and_xor_key() {
        let stream = resolve_flix_stream(&live_materials());
        let stream = stream.unwrap_or_else(|| panic!("the live chain resolves"));
        assert_eq!(stream.master_url, LIVE_MASTER_URL);
        assert_eq!(
            stream.xor_key_b64,
            "vit3U+SZD9kadR0DtPgEOY0g9/EgDPrh113Dxq6VNG4="
        );
    }

    #[test]
    fn fails_closed_on_bad_material() {
        let mut materials = live_materials();
        materials.seed = "zzzzzzzz".to_string();
        assert!(resolve_flix_stream(&materials).is_none());
        let mut materials = live_materials();
        materials.w_payload_b64 = "!!!".to_string();
        assert!(resolve_flix_stream(&materials).is_none());
        let mut materials = live_materials();
        materials.vid_b64 = "AAAA".to_string();
        assert!(resolve_flix_stream(&materials).is_none());
    }

    #[test]
    fn reads_leb128_values() {
        assert_eq!(read_leb_u32(&[0x00]), Some((0, 1)));
        assert_eq!(read_leb_u32(&[0x7f]), Some((127, 1)));
        assert_eq!(read_leb_u32(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(read_leb_u32(&[0xe5, 0x8e, 0x26]), Some((624_485, 3)));
    }
}
