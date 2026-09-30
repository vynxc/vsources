//! `HubExtractor`: the hub family front door.
//!
//! Ports `src/extractor/HubExtractor.js` — the dispatcher for hubcdn,
//! hubcloud, hubdrive, gdflix and gyanigurus links. It owns the
//! family's redirect-chain resolution (hubdrive pages → hubcloud links,
//! hubcdn `/dl/?link=` and `?r=BASE64` unwrapping) and its caches, and
//! delegates page extraction to [`HubCloud`]. `HBLinks` directory pages
//! hand their links to this extractor, so an
//! [`Arc<HubExtractor>`](std::sync::Arc) is shared between the two.
//!
//! Cache design: upstream keeps two in-memory maps — the resolution
//! cache (hubdrive/gyanigurus page → hubcloud link) and the hubcdn
//! cache (hubcdn link → resolved target) — each with a 5-minute TTL
//! (`HUBCLOUD_CACHE_TTL`) and eviction that drops *only expired*
//! entries once a map grows past 64 entries. That exact policy is why
//! this port uses `Mutex<HashMap<…>>` with timestamps rather than a
//! weighted cache: the "over threshold → purge only stale entries"
//! rule is not expressible with moka's size/weight policies, and the
//! maps are small (redirect chains per resolve).
//!
//! Task-41 (OOM fix), ported faithfully: a direct file URL (r2.dev /
//! media extension) or a googleusercontent CDN link is never delegated
//! to the hubcloud page chain — fetching those as text buffers the
//! whole video in memory (kernel OOM-kill observed upstream).
//!
//! Cut: upstream routes googleusercontent results through the addon's
//! `/range-proxy` (Google's `video-downloads.googleusercontent.com`
//! host lacks HTTP Range support, so seeking needs server-side Range
//! translation). This SDK has no server, so the direct URL ships
//! as-is; players lose byte-range seeking on that one host.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::language::find_country_codes;
use vsources_core::resolution::find_height;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

use super::hubcloud::{
    DEAD_HUBCLOUD_HOSTS, HUB_HOST_PATTERN, HUBCLOUD_CACHE_TTL, HubCloud, HubMeta, anchor_pairs,
    capture_nonempty, enrich_meta, is_cdn_direct_url, is_direct_file_url, parse_size_label,
    title_text,
};
use crate::helpers::{fetch_page_with, first_capture};

/// Eviction threshold for both in-memory caches
/// (`DEFAULT_EVICTION_THRESHOLD`).
const DEFAULT_EVICTION_THRESHOLD: usize = 64;

/// `<td>` cells, for the `File Size` row.
static SEL_TD: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td").unwrap_or_else(|e| panic!("valid td selector: {e}")));

static REURL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"var\s+reurl\s*=\s*["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid reurl pattern: {e}"))
});
static R_PARAM: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"[?&]r=([A-Za-z0-9+/=]+)")
        .unwrap_or_else(|e| panic!("valid r param pattern: {e}"))
});
static LINK_PARAM: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"[?&]link=(.+)$")
        .unwrap_or_else(|e| panic!("valid link param pattern: {e}"))
});
static VD_ANCHOR: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)<a\s+id=["']vd["']\s+href=["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid vd anchor pattern: {e}"))
});
static GDRIVE_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(https?://[^\s"'<>]*googleusercontent\.com[^\s"'<>]*)"#)
        .unwrap_or_else(|e| panic!("valid gdrive pattern: {e}"))
});
static HUBDRIVE_TITLE_PREFIX: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"^(HubDrive\s*\|\s*)")
        .unwrap_or_else(|e| panic!("valid hubdrive title pattern: {e}"))
});

/// The hub family dispatcher.
#[derive(Debug)]
pub struct HubExtractor {
    /// The page extractor all resolved links delegate to.
    hub_cloud: HubCloud,
    /// HubDrive/gyanigurus page → hubcloud link (upstream
    /// `resolutionCache`).
    resolution_cache: Mutex<HashMap<String, CacheEntry<ResolutionValue>>>,
    /// hubcdn link → resolved target (upstream `hubCdnCache`), shared
    /// between `normalize_async` and extraction to avoid a double
    /// fetch.
    hub_cdn_cache: Mutex<HashMap<String, CacheEntry<HubCdnResult>>>,
    /// Eviction threshold for both caches.
    eviction_threshold: usize,
}

impl Default for HubExtractor {
    fn default() -> Self {
        Self {
            hub_cloud: HubCloud::new(),
            resolution_cache: Mutex::new(HashMap::new()),
            hub_cdn_cache: Mutex::new(HashMap::new()),
            eviction_threshold: DEFAULT_EVICTION_THRESHOLD,
        }
    }
}

impl HubExtractor {
    /// A new extractor over its own [`HubCloud`]; fetches travel through
    /// the context, so only the redirect caches live here.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the cache eviction threshold (upstream constructor
    /// argument, used for OOM tuning).
    #[must_use]
    pub fn with_eviction_threshold(mut self, eviction_threshold: usize) -> Self {
        self.eviction_threshold = eviction_threshold;
        self
    }
}

impl HubExtractor {
    /// Ports `extractInternal` — the family's dispatch, callable with
    /// chain metadata by [`HBLinks`](super::hblinks::HBLinks).
    pub(crate) async fn extract_internal(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
        meta: &HubMeta,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let host = url.host_str().unwrap_or_default();
        if DEAD_HUBCLOUD_HOSTS.contains(&host) {
            return Err(ExtractorError::NotFound);
        }

        // HubCDN → may redirect to HubCloud (needs further extraction)
        // or a direct video URL.
        if host.contains("hubcdn") {
            let Some(result) = self.resolve_hub_cdn_url(ctx, url).await.ok().flatten() else {
                return Err(ExtractorError::NotFound);
            };
            if result.delegate_to_hub_cloud {
                return self
                    .hub_cloud
                    .extract_internal(ctx, &result.url, meta)
                    .await
                    .map_err(|_| ExtractorError::NotFound);
            }
            // True CDN direct URL. Upstream sends googleusercontent
            // links through the addon's /range-proxy (Range
            // translation) — cut, see the module docs.
            let is_google = is_cdn_direct_url(&result.url);
            let mut stream = Stream::new(
                result.url,
                if is_google {
                    Format::Mp4
                } else {
                    Format::Unknown
                },
            )
            .with_ttl(HUBCLOUD_CACHE_TTL)
            .with_label("HubCloud (CDN)");
            stream.meta.languages.clone_from(&meta.country_codes);
            stream.meta.resolution = meta.height;
            stream.meta.size = meta.size;
            stream.behavior_hints.insert(
                "bingeGroup".to_string(),
                format!("hub_cdn_{}", cdn_hash(url)),
            );
            return Ok(vec![stream]);
        }

        // Gyanigurus → resolve to hubdrive.tips, then delegate to the
        // hubdrive resolution.
        if host.contains("gyanigurus") {
            if let Some(cached) = self.cached_resolution(url.as_str()) {
                // `{ ...cached.meta, ...meta }` — the plain spread.
                let enriched = HubMeta {
                    country_codes: meta.country_codes.clone(),
                    height: meta.height.or(cached.meta.height),
                    size: meta.size.or(cached.meta.size),
                };
                return self
                    .extract_via_hub_cloud(ctx, &cached.url, &enriched)
                    .await;
            }

            // Fetch the page and find the hubdrive.tips link.
            let Ok(html) = fetch_page_with(ctx, url, url).await else {
                return Err(ExtractorError::NotFound);
            };
            let hub_drive_url = {
                let document = Html::parse_document(&html);
                anchor_pairs(&document)
                    .into_iter()
                    .find_map(|(_text, href)| {
                        let href = href?;
                        href.contains("hubdrive").then_some(href)
                    })
            };
            if let Some(href) = hub_drive_url {
                // `new URL(hubDriveUrl)` — a relative link is the
                // upstream try/catch's miss.
                if let Ok(resolved) = Url::parse(&href) {
                    self.store_resolution(
                        url.as_str(),
                        ResolutionValue {
                            url: resolved.clone(),
                            meta: HubMeta::default(),
                        },
                    );
                    return self.extract_via_hub_cloud(ctx, &resolved, meta).await;
                }
            }
            return Err(ExtractorError::NotFound);
        }

        // HubDrive → always fresh extraction (skipping the resolution
        // cache avoids sourceId conflicts when multiple sources return
        // the same hubdrive URL).
        if host.contains("hubdrive") {
            return self.extract_via_hub_cloud(ctx, url, meta).await;
        }

        // HubCloud → delegate directly.
        self.hub_cloud.extract_internal(ctx, url, meta).await
    }

    /// Extract via a `HubDrive` page: resolve it to its hubcloud link and
    /// delegate (upstream `extractViaHubCloud`).
    async fn extract_via_hub_cloud(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
        meta: &HubMeta,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);
        let Ok(html) = fetch_page_with(ctx, url, referer).await else {
            return Err(ExtractorError::NotFound);
        };
        let (hub_cloud_url, hub_drive_meta) = {
            let document = Html::parse_document(&html);
            match find_hub_cloud_url(&document) {
                Some(url) => (url, extract_hub_drive_meta(&document)),
                None => return Err(ExtractorError::NotFound),
            }
        };
        let enriched = enrich_meta(&hub_drive_meta, meta);
        self.hub_cloud
            .extract_internal(ctx, &hub_cloud_url, &enriched)
            .await
            .map_err(|_| ExtractorError::NotFound)
    }

    /// Resolve a `HubDrive` (or gyanigurus) page to its hubcloud link and
    /// page metadata (upstream `resolveHubDriveToHubCloud`); `None` on a
    /// failed fetch or a page without a live hubcloud link.
    async fn resolve_hub_drive_to_hub_cloud(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Option<ResolutionValue> {
        let html = fetch_page_with(ctx, url, url).await.ok()?;
        let document = Html::parse_document(&html);
        Some(ResolutionValue {
            url: find_hub_cloud_url(&document)?,
            meta: extract_hub_drive_meta(&document),
        })
    }

    /// Resolve a hubcdn URL, through the in-memory cache that keeps
    /// `normalize_async` and extraction from double-fetching.
    async fn resolve_hub_cdn_url(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Option<HubCdnResult>, ExtractorError> {
        if let Some(cached) = self.cached_hub_cdn(url.as_str()) {
            return Ok(Some(cached));
        }
        let html = fetch_page_with(ctx, url, url).await?;
        let result = extract_hub_cdn_url(&html);
        if let Some(value) = result.clone() {
            self.store_hub_cdn(url.as_str(), value);
        }
        Ok(result)
    }

    /// A fresh (TTL-valid) resolution-cache entry.
    fn cached_resolution(&self, key: &str) -> Option<ResolutionValue> {
        let cache = self
            .resolution_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(key)
            .filter(|entry| entry.ts.elapsed() < HUBCLOUD_CACHE_TTL)
            .map(|entry| entry.value.clone())
    }

    /// Store a resolution entry, evicting expired entries past the
    /// threshold (upstream `evictExpired`).
    fn store_resolution(&self, key: &str, value: ResolutionValue) {
        let mut cache = self
            .resolution_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            key.to_string(),
            CacheEntry {
                value,
                ts: Instant::now(),
            },
        );
        if cache.len() > self.eviction_threshold {
            evict_expired(&mut cache);
        }
    }

    /// A fresh (TTL-valid) hubcdn-cache entry.
    fn cached_hub_cdn(&self, key: &str) -> Option<HubCdnResult> {
        let cache = self
            .hub_cdn_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(key)
            .filter(|entry| entry.ts.elapsed() < HUBCLOUD_CACHE_TTL)
            .map(|entry| entry.value.clone())
    }

    /// Store a hubcdn entry, evicting expired entries past the
    /// threshold.
    fn store_hub_cdn(&self, key: &str, value: HubCdnResult) {
        let mut cache = self
            .hub_cdn_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            key.to_string(),
            CacheEntry {
                value,
                ts: Instant::now(),
            },
        );
        if cache.len() > self.eviction_threshold {
            evict_expired(&mut cache);
        }
    }
}

/// A TTL-stamped cache value — the upstream `{ value, ts }` entries.
#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    ts: Instant,
}

/// A resolved hubdrive page: its hubcloud link plus page metadata.
#[derive(Debug, Clone)]
struct ResolutionValue {
    url: Url,
    meta: HubMeta,
}

/// A resolved hubcdn URL and whether it needs `HubCloud` extraction.
#[derive(Debug, Clone)]
struct HubCdnResult {
    url: Url,
    delegate_to_hub_cloud: bool,
}

impl HubCdnResult {
    /// A result whose delegation follows `shouldDelegateToHubCloud`.
    fn delegate(url: Url) -> Self {
        Self {
            delegate_to_hub_cloud: should_delegate_to_hub_cloud(&url),
            url,
        }
    }
}

/// Delegation is only for PAGES needing redirect-chain parsing
/// (Task-41): true CDN hosts and direct file URLs stay as-is.
fn should_delegate_to_hub_cloud(url: &Url) -> bool {
    !is_cdn_direct_url(url) && !is_direct_file_url(url)
}

/// Drop entries older than the TTL — upstream `evictExpired`.
fn evict_expired<T>(cache: &mut HashMap<String, CacheEntry<T>>) {
    cache.retain(|_, entry| entry.ts.elapsed() < HUBCLOUD_CACHE_TTL);
}

/// FNV-1a hash of the URL pathname, top 16 bits as four hex digits —
/// upstream `cdnHash`, the per-CDN-link binge-group key.
fn cdn_hash(url: &Url) -> String {
    // `charCodeAt` iterates UTF-16 code units.
    let mut hash: u32 = 0x811c_9dc5;
    for unit in url.path().encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:04x}", hash >> 16)
}

/// Strip the query for a canonical cache key — upstream
/// `stripQueryParams`.
fn strip_query_params(url: &Url) -> Url {
    let mut canonical = url.clone();
    canonical.set_query(None);
    canonical
}

/// The first live hubcloud link on a hubdrive page — upstream
/// `findHubCloudUrl`: the first anchor whose text contains `HubCloud`,
/// skipping dead hosts and unusable hrefs (jQuery's `map` skips null
/// returns, so later anchors are still tried).
fn find_hub_cloud_url(document: &Html) -> Option<Url> {
    anchor_pairs(document).into_iter().find_map(|(text, href)| {
        if !text.contains("HubCloud") {
            return None;
        }
        let href = href?;
        let parsed = Url::parse(&href).ok()?;
        let host = parsed.host_str().unwrap_or_default();
        if DEAD_HUBCLOUD_HOSTS.contains(&host) {
            return None;
        }
        Some(parsed)
    })
}

/// Metadata off a `HubDrive` page (upstream `extractHubDriveMeta`): the
/// title (with the `HubDrive |` prefix stripped), its language and
/// resolution tags, and the `File Size` cell.
fn extract_hub_drive_meta(document: &Html) -> HubMeta {
    let page_title = {
        let raw = title_text(document);
        let stripped = match first_capture(&HUBDRIVE_TITLE_PREFIX, &raw).ok().flatten() {
            Some(prefix) => raw.replacen(&prefix, "", 1),
            None => raw,
        };
        stripped.trim().to_string()
    };
    // `$('td').filter(text === 'File Size').next().text()`
    let file_size_text = document
        .select(&SEL_TD)
        .find(|element| element.text().collect::<String>().trim() == "File Size")
        .and_then(|element| element.next_sibling().and_then(ElementRef::wrap))
        .map(|next| next.text().collect::<String>().trim().to_string())
        .unwrap_or_default();
    HubMeta {
        country_codes: find_country_codes(&page_title),
        height: find_height(&page_title),
        size: parse_size_label(&file_size_text),
    }
}

/// Unified `HubCDN` extraction — upstream `extractHubCdnUrl`, handling
/// `var reurl`, `/dl/?link=`, `?r=BASE64`, `<a id="vd">`, and the
/// googleusercontent fallback.
fn extract_hub_cdn_url(html: &str) -> Option<HubCdnResult> {
    // Pattern 1: var reurl = "…"
    if let Some(reurl_value) = capture_nonempty(&REURL, html) {
        // 1a: /dl/?link=URL → the link param.
        if reurl_value.contains("hubcdn")
            && reurl_value.contains("/dl/?link=")
            && let Ok(parsed) = Url::parse(&reurl_value)
            && let Some(link_param) = query_param(&parsed, "link")
            && let Ok(target) = Url::parse(&link_param)
        {
            return Some(HubCdnResult::delegate(target));
        }

        // 1b: ?r=BASE64 → decode (alternative mirror format).
        if let Some(encoded) = capture_nonempty(&R_PARAM, &reurl_value)
            && let Some(result) = decode_r_param(&encoded)
        {
            return Some(result);
        }

        // 1c: a plain URL (direct video URL — skip self-referential
        // hubcdn/dl/ URLs).
        if !reurl_value.contains("/dl/?link=")
            && let Ok(direct_url) = Url::parse(&reurl_value)
        {
            return Some(HubCdnResult::delegate(direct_url));
        }
    }

    // Pattern 2: <a id="vd" href='URL'>.
    if let Some(href) = capture_nonempty(&VD_ANCHOR, html)
        && let Ok(vd_url) = Url::parse(&href)
    {
        return Some(HubCdnResult::delegate(vd_url));
    }

    // Pattern 3: any googleusercontent.com URL — always CDN direct.
    if let Some(gdrive_url) = capture_nonempty(&GDRIVE_URL, html)
        && let Ok(url) = Url::parse(&gdrive_url)
    {
        return Some(HubCdnResult {
            url,
            delegate_to_hub_cloud: false,
        });
    }

    None
}

/// A `?r=` parameter's base64 payload, decoded — the link param (if
/// percent-encoded) or the URL itself.
fn decode_r_param(encoded: &str) -> Option<HubCdnResult> {
    use base64::Engine as _;
    let decoded_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = std::str::from_utf8(&decoded_bytes).ok()?;
    let url = match first_capture(&LINK_PARAM, decoded).ok().flatten() {
        Some(link) => {
            let decoded_link = decode_uri_component(&link)?;
            Url::parse(&decoded_link).ok()?
        }
        None => Url::parse(decoded).ok()?,
    };
    Some(HubCdnResult::delegate(url))
}

/// `decodeURIComponent` for the hubcdn link param: `%XX` sequences as
/// UTF-8, `None` on malformed escapes (upstream's `URIError` path).
fn decode_uri_component(value: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1])?;
                let low = hex(bytes[index + 2])?;
                out.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `searchParams.get(name)`.
fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[async_trait]
impl Extractor for HubExtractor {
    fn id(&self) -> &'static str {
        "hub"
    }

    fn label(&self) -> &'static str {
        "HubCloud"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HUB_HOST_PATTERN.is_match(host).unwrap_or(false))
    }

    /// Resolve to the canonical form for the cache key: hubcdn →
    /// resolved hubcloud (or as-is for direct video hosts), hubcloud →
    /// `?token=` stripped, hubdrive → resolved and stripped.
    async fn normalize_async(&self, ctx: &ResolveCtx<'_>, url: &Url) -> Url {
        let host = url.host_str().unwrap_or_default();

        // HubCDN: resolve→hubcloud canonical, or as-is for direct video
        // hosts.
        if host.contains("hubcdn") {
            if let Ok(Some(result)) = self.resolve_hub_cdn_url(ctx, url).await
                && result.delegate_to_hub_cloud
            {
                return strip_query_params(&result.url);
            }
            return url.clone();
        }

        // HubCloud: strip the ephemeral ?token= for the canonical cache
        // key only.
        if host.contains("hubcloud") {
            return strip_query_params(url);
        }

        // HubDrive: resolve→hubcloud, then strip query params.
        if let Some(cached) = self.cached_resolution(url.as_str()) {
            return strip_query_params(&cached.url);
        }
        match self.resolve_hub_drive_to_hub_cloud(ctx, url).await {
            Some(resolved) => {
                self.store_resolution(url.as_str(), resolved.clone());
                strip_query_params(&resolved.url)
            }
            None => url.clone(),
        }
    }

    fn cache_version(&self) -> Option<u32> {
        Some(2)
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        self.extract_internal(ctx, url, &HubMeta::default()).await
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, ctx_for};

    use super::*;

    /// A hubdrive page: the title carries the metadata, the table the
    /// file size, the anchor the live hubcloud link.
    const HUBDRIVE_PAGE: &str = concat!(
        r#"<html><head><title>HubDrive | Movie 1080p English</title></head><body>"#,
        r#"<table><tr><td>File Size</td><td>2.1 GB</td></tr></table>"#,
        r#"<a href="https://hubcloud.one/file/xyz">HubCloud</a>"#,
        r#"<a href="https://hubcloud.ink/dead">HubCloud</a>"#,
        r#"</body></html>"#,
    );

    /// A hubcloud redirect page (hop 1) and download page (hop 2).
    const HUBCLOUD_REDIRECT: &str = r#"<script>var url = "/dl/xyz";</script>"#;
    const HUBCLOUD_DOWNLOAD: &str = concat!(
        r#"<html><head><title>Movie 1080p English</title></head><body>"#,
        r#"<div id="size">1.4 GB</div>"#,
        r#"<a href="https://hubcloud.one/workers/file1">Download File</a>"#,
        r#"</body></html>"#,
    );

    /// The hub family fixture chain: hubdrive page + hubcloud hops.
    fn hub_family(fetcher: ScriptedFetcher) -> ScriptedFetcher {
        fetcher
            .page("/drive/abc", HUBDRIVE_PAGE)
            .page("/file/xyz", HUBCLOUD_REDIRECT)
            .page("/dl/xyz", HUBCLOUD_DOWNLOAD)
    }

    fn hubdrive_url() -> Url {
        Url::parse("https://hubdrive.pics/drive/abc").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    fn hubcloud_url() -> Url {
        Url::parse("https://hubcloud.one/file/xyz").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_hub_family() {
        let extractor = HubExtractor::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/x"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("hubcloud.one"));
        assert!(supports("hubcdn.club"));
        assert!(supports("hubdrive.pics"));
        assert!(supports("gdflix.click"));
        assert!(supports("gyanigurus.link"));
        assert!(!supports("example.com"));
        assert!(!supports("hblinks.co"));
    }

    #[test]
    fn reports_the_upstream_cache_version() {
        assert_eq!(HubExtractor::new().cache_version(), Some(2));
    }

    #[test]
    fn hashes_cdn_paths_with_fnv1a() {
        // FNV-1a of "/file/cdn1", high 16 bits: b910.
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert_eq!(cdn_hash(&url), "b910");
        assert_eq!(cdn_hash(&hubcloud_url()), "0622");
    }

    #[test]
    fn delegation_skips_cdn_and_direct_file_urls() {
        let parse = |value: &str| Url::parse(value).unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert!(!should_delegate_to_hub_cloud(&parse(
            "https://file-1.googleusercontent.com/a"
        )));
        assert!(!should_delegate_to_hub_cloud(&parse(
            "https://file.r2.dev/video.mkv"
        )));
        assert!(should_delegate_to_hub_cloud(&parse(
            "https://hubcloud.one/file/xyz"
        )));
    }

    #[tokio::test]
    async fn dead_hubcloud_hosts_are_misses_without_fetching() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcloud.ink/file/xyz")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match HubExtractor::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a dead hubcloud host must be a NotFound, got {other:?}"),
        }
        assert!(fetcher.requests().is_empty());
    }

    #[tokio::test]
    async fn normalize_strips_tokens_from_hubcloud_urls() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcloud.one/file/xyz?token=ephemeral")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let canonical = HubExtractor::new()
            .normalize_async(&ctx, &url)
            .await
            .as_str()
            .to_string();
        assert_eq!(canonical, "https://hubcloud.one/file/xyz");
    }

    #[tokio::test]
    async fn normalize_resolves_hubdrive_to_a_canonical_hubcloud_url() {
        let fetcher = hub_family(ScriptedFetcher::default());
        let ctx = ctx_for(&fetcher, None);

        let extractor = HubExtractor::new();
        let canonical = extractor
            .normalize_async(&ctx, &hubdrive_url())
            .await
            .as_str()
            .to_string();
        assert_eq!(canonical, "https://hubcloud.one/file/xyz");
        // The resolution cache serves the second call.
        let again = extractor
            .normalize_async(&ctx, &hubdrive_url())
            .await
            .as_str()
            .to_string();
        assert_eq!(again, canonical);
        let fetch_count = fetcher
            .requests()
            .iter()
            .filter(|request| request.url.path() == "/drive/abc")
            .count();
        assert_eq!(
            fetch_count, 1,
            "the second call must hit the resolution cache"
        );
    }

    #[tokio::test]
    async fn normalize_keeps_hubcdn_urls_that_resolve_directly() {
        let fetcher = ScriptedFetcher::default().page(
            "/file/cdn1",
            r#"<script>var reurl = "https://file.r2.dev/video.mkv";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        // A direct file target: the hubcdn URL stays as-is.
        let canonical = HubExtractor::new().normalize_async(&ctx, &url).await;
        assert_eq!(canonical.as_str(), "https://hubcdn.club/file/cdn1");
    }

    #[tokio::test]
    async fn hubdrive_pages_extract_via_hubcloud() {
        let fetcher = hub_family(ScriptedFetcher::default());
        // The provider page that linked to hubdrive — the context
        // referer upstream calls `meta.referer`.
        let provider = Url::parse("https://provider.example/movie")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        let ctx = ctx_for(&fetcher, Some(&provider));
        let streams = HubExtractor::new()
            .extract(&ctx, &hubdrive_url())
            .await
            .unwrap_or_else(|e| panic!("the hubdrive chain must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("HubCloud (Download)"));
        assert_eq!(stream.url.as_str(), "https://hubcloud.one/workers/file1");
        assert_eq!(stream.ttl, HUBCLOUD_CACHE_TTL);
        assert_eq!(
            stream.meta.languages,
            vec![vsources_core::types::CountryCode::En]
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        // The hubcloud page's #size overrides the hubdrive File Size
        // cell (upstream `bytes: fileSize` spreads over the chain meta).
        assert_eq!(stream.meta.size, Some(1_503_238_553));
        assert_eq!(
            stream.behavior_hints.get("bingeGroup").map(String::as_str),
            Some("hubcloud_direct")
        );
        // Every hubdrive/hubcloud hop carries the provider page as
        // its Referer (upstream `meta.referer ?? url.href`), except the
        // download page, which keeps the hubcloud link itself.
        assert_eq!(
            fetcher.sent_header("/drive/abc", "Referer").as_deref(),
            Some("https://provider.example/movie")
        );
        assert_eq!(
            fetcher.sent_header("/file/xyz", "Referer").as_deref(),
            Some("https://provider.example/movie")
        );
        assert_eq!(
            fetcher.sent_header("/dl/xyz", "Referer").as_deref(),
            Some("https://hubcloud.one/file/xyz")
        );
    }

    #[tokio::test]
    async fn hubdrive_pages_without_live_hubcloud_links_are_misses() {
        let fetcher = ScriptedFetcher::default().page(
            "/drive/abc",
            r#"<html><head><title>HubDrive | Movie 1080p</title></head><body><a href="https://hubcloud.ink/dead">HubCloud</a></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        match HubExtractor::new().extract(&ctx, &hubdrive_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a dead hubcloud link must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hubcdn_direct_files_never_delegate() {
        let fetcher = ScriptedFetcher::default().page(
            "/file/cdn1",
            r#"<script>var reurl = "https://file.r2.dev/video.mkv";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the hubcdn card must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.url.as_str(), "https://file.r2.dev/video.mkv");
        assert_eq!(stream.format, Format::Unknown);
        assert_eq!(stream.label.as_deref(), Some("HubCloud (CDN)"));
        assert_eq!(
            stream.behavior_hints.get("bingeGroup").map(String::as_str),
            Some("hub_cdn_b910")
        );
        // Task-41: only the hubcdn page was fetched — the file itself
        // never travels.
        assert_eq!(fetcher.requests().len(), 1);
        assert_eq!(fetcher.requests()[0].url.path(), "/file/cdn1");
    }

    #[tokio::test]
    async fn hubcdn_google_links_ship_as_direct_mp4() {
        let fetcher = ScriptedFetcher::default().page(
            "/file/cdn1",
            r#"<script>var reurl = "https://file-1.googleusercontent.com/a/b";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the google cdn card must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://file-1.googleusercontent.com/a/b"
        );
        assert_eq!(streams[0].format, Format::Mp4);
        assert_eq!(streams[0].label.as_deref(), Some("HubCloud (CDN)"));
    }

    #[tokio::test]
    async fn hubcdn_dl_link_params_delegate_to_hubcloud() {
        let fetcher = hub_family(ScriptedFetcher::default()).page(
            "/file/cdn1",
            r#"<script>var reurl = "https://hubcdn.club/dl/?link=https://hubcloud.one/file/xyz?token=t1";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the dl link must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );

        // normalize_async canonicalizes through the same resolution.
        let canonical = HubExtractor::new().normalize_async(&ctx, &url).await;
        assert_eq!(canonical.as_str(), "https://hubcloud.one/file/xyz");
    }

    #[tokio::test]
    async fn hubcdn_r_parameters_decode_to_hubcloud_links() {
        // base64("https://hubcloud.one/file/xyz")
        let fetcher = hub_family(ScriptedFetcher::default()).page(
            "/file/cdn1",
            r#"<script>var reurl = "https://hubcdn.club/dl/?r=aHR0cHM6Ly9odWJjbG91ZC5vbmUvZmlsZS94eXo=";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the r param must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
    }

    #[tokio::test]
    async fn hubcdn_r_parameters_decode_percent_encoded_links() {
        // base64("https://hubcloud.one/dl/?link=https%3A%2F%2Fhubcloud.one%2Ffile%2Fxyz")
        let fetcher = hub_family(ScriptedFetcher::default()).page(
            "/file/cdn1",
            r#"<script>var reurl = "https://hubcdn.club/dl/?r=aHR0cHM6Ly9odWJjbG91ZC5vbmUvZGwvP2xpbms9aHR0cHMlM0ElMkYlMkZodWJjbG91ZC5vbmUlMkZmaWxlJTJGeHl6";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the encoded r param must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
    }

    #[tokio::test]
    async fn hubcdn_vd_anchors_delegate() {
        let fetcher = hub_family(ScriptedFetcher::default()).page(
            "/file/cdn1",
            r#"<a id="vd" href='https://hubcloud.one/file/xyz'>Download</a>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the vd anchor must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
    }

    #[tokio::test]
    async fn hubcdn_googleusercontent_fallback_is_direct() {
        let fetcher = ScriptedFetcher::default().page(
            "/file/cdn1",
            r#"<p>no reurl here</p><a href="https://file-9.googleusercontent.com/x">g</a>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the gdrive fallback must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://file-9.googleusercontent.com/x"
        );
        assert_eq!(streams[0].format, Format::Mp4);
    }

    #[tokio::test]
    async fn hubcdn_pages_without_a_pattern_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/file/cdn1", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hubcdn.club/file/cdn1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match HubExtractor::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a pattern-less hubcdn page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gyanigurus_pages_resolve_through_hubdrive() {
        let fetcher = hub_family(ScriptedFetcher::default()).page(
            "/g/1",
            r#"<html><body><a href="https://hubdrive.pics/drive/abc">Watch</a></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url =
            Url::parse("https://gyanigurus.link/g/1").unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubExtractor::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the gyanigurus chain must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
        // The gyanigurus page carried itself as Referer.
        assert_eq!(
            fetcher.sent_header("/g/1", "Referer").as_deref(),
            Some("https://gyanigurus.link/g/1")
        );
    }

    #[tokio::test]
    async fn gyanigurus_pages_without_hubdrive_links_are_misses() {
        let fetcher = ScriptedFetcher::default().page(
            "/g/1",
            r#"<html><body><a href="https://example.com">nope</a></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url =
            Url::parse("https://gyanigurus.link/g/1").unwrap_or_else(|e| panic!("valid URL: {e}"));

        match HubExtractor::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a hubdrive-less page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_hubcloud_urls_delegate_directly() {
        let fetcher = hub_family(ScriptedFetcher::default());
        let ctx = ctx_for(&fetcher, None);

        let streams = HubExtractor::new()
            .extract(&ctx, &hubcloud_url())
            .await
            .unwrap_or_else(|e| panic!("the hubcloud link must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
    }
}
