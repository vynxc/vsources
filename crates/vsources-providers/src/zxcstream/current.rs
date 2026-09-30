//! Current player.zxcprime.xyz protocol, observed 2026-09-26.
//! The live player posts obfuscated field names, then decrypts `CryptoJS` links.

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::*;

const BASE: &str = "https://player.zxcprime.xyz";
const TOKEN_ROUTE: &str = "/backend_/tanginamogagotarantado";
// Public passphrase embedded in the site's CryptoJS player bundle.
const PASSPHRASE: &[u8] = b"7f4c9e2a81d63b05c4f7a9e8126d3b50e1a8c7f23d9465ab0c6e9f1d4a7b832c";
const TITLE: &str = "5e28c9147a306d531e829f3674b392a1";
const YEAR: &str = "b731e6c94f082a169d725f8341c306e";
const DATE: &str = "e164932c50216ad739e5814b3027";
const SERVERS: &[&str] = &[
    "berkas", "valstrax", "atlas", "alatreon", "daedalus", "resshin",
];

fn request(url: Url) -> FetchRequest {
    FetchRequest::get(url)
        .with_header("Origin", BASE)
        .with_header("Referer", format!("{BASE}/"))
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json, text/plain, */*")
        .with_timeout(Duration::from_secs(6))
}

pub(super) async fn resolve(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    id: u64,
    name: &str,
    year: Option<u16>,
) -> Vec<Stream> {
    let tasks = SERVERS
        .iter()
        .map(|server| resolve_server(ctx, media, id, name, year, server));
    let results = futures::future::join_all(tasks).await;
    let mut seen = std::collections::HashSet::new();
    results
        .into_iter()
        .flatten()
        .filter(|s| seen.insert(s.url.clone()))
        .collect()
}

async fn resolve_server(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    id: u64,
    name: &str,
    year: Option<u16>,
    server: &str,
) -> Vec<Stream> {
    async fn load(
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        id: u64,
        name: &str,
        year: Option<u16>,
        server: &str,
    ) -> Option<Vec<Stream>> {
        let kind = if media.season.is_some() {
            "tv"
        } else {
            "movie"
        };
        let mut body = json!({field_map::ID:id.to_string(), field_map::MEDIA_TYPE:kind, field_map::PATH:server});
        if let Some(season) = media.season {
            body[field_map::SEASON] = json!(season);
            body[field_map::EPISODE] = json!(media.episode.unwrap_or(1));
        }
        let mut token_request = request(Url::parse(&format!("{BASE}{TOKEN_ROUTE}")).ok()?)
            .with_header("Content-Type", "application/json");
        token_request.method = "POST".into();
        token_request.body = Some(body.to_string());
        let response = ctx.fetcher.request(token_request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let token: Value = response.json().ok()?;
        let mut url = Url::parse(&format!("{BASE}/backend_/sources/{server}")).ok()?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair(field_map::ID, &id.to_string())
                .append_pair(field_map::PATH, server)
                .append_pair(field_map::MEDIA_TYPE, kind)
                .append_pair(field_map::TS, &token.get("ts")?.to_string())
                .append_pair(field_map::TOKEN, token.get("token")?.as_str()?)
                .append_pair(TITLE, name)
                .append_pair(YEAR, &year.map(|y| y.to_string()).unwrap_or_default())
                .append_pair(DATE, &year.map(|y| y.to_string()).unwrap_or_default());
            if let Some(season) = media.season {
                q.append_pair(field_map::SEASON, &season.to_string())
                    .append_pair(field_map::EPISODE, &media.episode.unwrap_or(1).to_string());
            }
        }
        let response = ctx.fetcher.request(request(url)).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let data: Value = response.json().ok()?;
        Some(cards(&data, server))
    }
    load(ctx, media, id, name, year, server)
        .await
        .unwrap_or_default()
}

fn cards(data: &Value, server: &str) -> Vec<Stream> {
    let Some(links) = data.get("links").and_then(Value::as_array) else {
        return Vec::new();
    };
    links
        .iter()
        .filter_map(|link| {
            let plain = decrypt_link(link.get("link")?.as_str()?)?;
            let url = Url::parse(&plain).ok()?;
            if !matches!(url.scheme(), "https" | "http") {
                return None;
            }
            let format = match link.get("type").and_then(Value::as_str) {
                Some("hls") => vsources_core::types::Format::Hls,
                Some("mp4" | "mkv") => vsources_core::types::Format::Mp4,
                _ => vsources_extractors::helpers::format_for_url(&url),
            };
            let mut stream = Stream::new(url, format)
                .with_ttl(TTL)
                .with_label(format!("ZXCStream · {server}"));
            stream.meta.source_id = Some("zxcstream".into());
            stream.meta.source_label = Some("ZXCStream".into());
            stream.meta.request_headers = BTreeMap::from([
                ("Referer".into(), format!("{BASE}/")),
                ("Origin".into(), BASE.into()),
                ("User-Agent".into(), UA.into()),
            ]);
            stream.meta.resolution = link
                .get("resolution")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0);
            Some(stream)
        })
        .collect()
}

fn decrypt_link(encoded: &str) -> Option<String> {
    let body = STANDARD.decode(encoded).ok()?;
    if !body.starts_with(b"Salted__") || body.len() < 32 {
        return None;
    }
    let salt = body.get(8..16)?;
    let mut material = Vec::new();
    let mut previous = Vec::new();
    while material.len() < 48 {
        let mut digest = Md5::new();
        digest.update(&previous);
        digest.update(PASSPHRASE);
        digest.update(salt);
        previous = digest.finalize().to_vec();
        material.extend_from_slice(&previous);
    }
    let cipher =
        cbc::Decryptor::<aes::Aes256>::new_from_slices(&material[..32], &material[32..48]).ok()?;
    let mut encrypted = body[16..].to_vec();
    let plain = cipher.decrypt_padded_mut::<Pkcs7>(&mut encrypted).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Independent OpenSSL EVP_BytesToKey / AES-CBC fixture, generated with Node.
    const ENCRYPTED: &str = "U2FsdGVkX18BAgMEBQYHCOe2EGJOVtWS2ICN9AqrMm4pdONus0eekdpLINwpFRfx";
    #[test]
    fn decrypts_current_cryptojs_envelope_and_preserves_format() {
        assert_eq!(
            decrypt_link(ENCRYPTED).as_deref(),
            Some("https://cdn.example/opaque")
        );
        let data = json!({"links":[{"link":ENCRYPTED,"type":"hls","resolution":1080}]});
        let streams = cards(&data, "berkas");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, vsources_core::types::Format::Hls);
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert!(decrypt_link("garbage").is_none());
    }
}
