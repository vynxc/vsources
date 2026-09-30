//! Shared helpers for host extractors.
//!
//! The host extractor files each port one upstream `src/extractor/*.js`
//! class; the repeated glue — Referer resolution, host matching, direct
//! stream construction, common regular expressions — lives here so the
//! ports stay close to their sources.

use std::collections::BTreeMap;

use url::Url;
use vsources_core::traits::{FetchRequest, ResolveCtx};
use vsources_core::types::Format;
use vsources_core::types::Stream;

/// A case-insensitive host matcher, ports `url.host.match(/…/)`.
macro_rules! host_matcher {
    ($name:ident, $pattern:literal) => {
        static $name: std::sync::LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
            fancy_regex::Regex::new($pattern).unwrap_or_else(|e| panic!("valid host pattern: {e}"))
        });
    };
}
pub(crate) use host_matcher;

/// Fetch headers carrying `Referer: referer`.
///
/// Hosts gate hotlinked media on the embedding page; the referer is
/// `ctx.referer` when the context knows it, the embed itself otherwise
/// (resolve that with `ctx.referer.unwrap_or(url)`).
pub fn referer_headers(ctx: &ResolveCtx<'_>, referer: &Url) -> BTreeMap<String, String> {
    let _ = ctx;
    BTreeMap::from([("Referer".to_string(), referer.to_string())])
}

/// Fetch a page whose Referer is the context's embedding page (or the
/// page itself).
pub async fn fetch_page(
    ctx: &ResolveCtx<'_>,
    url: &Url,
) -> Result<String, vsources_core::error::ExtractorError> {
    let referer = ctx.referer.unwrap_or(url);
    fetch_page_with(ctx, url, referer).await
}

/// Fetch a page with an explicit Referer.
///
/// Multi-step hosts (`DoodStream`'s `/pass_md5/` hop) keep the embed page
/// as the Referer for every request they make.
pub async fn fetch_page_with(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &Url,
) -> Result<String, vsources_core::error::ExtractorError> {
    let mut request = FetchRequest::get(url.clone());
    for (name, value) in referer_headers(ctx, referer) {
        request = request.with_header(name, value);
    }
    Ok(ctx.fetcher.request(request).await?.body)
}

/// A direct stream whose player must send `Referer: origin + "/"`.
///
/// The dominant result shape across the upstream host extractors: one
/// playable URL plus the hotlink Referer of the page it came from.
pub fn direct_stream(url: Url, format: Format, ttl: std::time::Duration, origin: &Url) -> Stream {
    // `requestHeaders: { Referer: url.origin + '/' }` — the hotlink
    // referer is the scheme and host, not the full page URL.
    let referer = format!(
        "{}://{}/",
        origin.scheme(),
        origin.host_str().unwrap_or_default()
    );
    Stream::new(url, format).with_ttl(ttl).with_referer(referer)
}

/// The format for a direct URL, from its extension.
pub fn format_for_url(url: &Url) -> Format {
    match url
        .path()
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("m3u8") => Format::Hls,
        Some("mp4" | "mkv" | "webm") => Format::Mp4,
        _ => Format::Unknown,
    }
}

/// The first capture group of `regex` in `text`.
pub fn first_capture(
    regex: &fancy_regex::Regex,
    text: &str,
) -> Result<Option<String>, fancy_regex::Error> {
    Ok(regex
        .captures(text)?
        .and_then(|caps| caps.get(1).map(|group| group.as_str().to_string())))
}

/// An alphanumeric random string of `len` chars, ports
/// `Math.random().toString(36).substring(2, 12)`.
pub fn random_token(len: usize) -> String {
    // Dood's direct URLs mix in a random component to defeat naive
    // caching; timestamp entropy is plenty for that purpose.
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut value = nanos;
    let mut out = String::with_capacity(len);
    while out.len() < len {
        out.push(ALPHABET[(value % ALPHABET.len() as u128) as usize] as char);
        value = value
            .checked_div(ALPHABET.len() as u128)
            .unwrap_or_default();
        if value == 0 {
            value = nanos.rotate_left(17) ^ u128::from(len as u64);
        }
    }
    out
}
