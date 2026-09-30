//! `HubCloud`: download-button pages behind hubcloud links.
//!
//! Ports `src/extractor/HubCloud.js` and the family-shared pieces of
//! `src/utils/hub.js` (`HUB_HOST_PATTERN`, `DEAD_HUBCLOUD_HOSTS`,
//! `HUBCLOUD_CACHE_TTL`). A hubcloud URL serves a redirect page whose
//! script hands over the real download page; that page lists one button
//! per mirror, classified here into the upstream `SERVER_CATEGORIES`
//! and shipped as direct streams with a 5-minute TTL (the mirror URLs
//! carry short-lived session tokens).
//!
//! This module is also the family's shared home: [`HubExtractor`] and
//! [`HBLinks`](super::hblinks::HBLinks) import the pattern, the TTL, the
//! dead-host list, `HubMeta`, `is_direct_file_url`, and
//! `parse_size_label` from here, mirroring how upstream shares them out
//! of `utils/hub.js`.
//!
//! Port notes (upstream features cut, with reasons):
//!
//! - **`fetcher.setCookie` → explicit `Cookie` header.** Upstream plants
//!   the `stck`-derived cookie in the fetcher's shared jar; the
//!   [`Fetcher`](vsources_core::traits::Fetcher) trait has no jar access,
//!   so the cookie travels as a `Cookie` header on the download-page
//!   request — the identical wire request.
//! - **`meta.title` is cut.** `StreamMeta` has no title field; the
//!   `⚠️ no seek` marker upstream appended to the title of non-seekable
//!   categories survives as a `noSeek` entry in
//!   [`Stream::behavior_hints`], and the upstream `meta.extractorId`
//!   (the `StreamResolver` binge-group key) becomes the `bingeGroup` hint.
//! - **Category `priority` values are cut.** Upstream declares them but
//!   never reads them — server routing leftovers.
//! - The retry after an invalid download page waits the upstream
//!   `RETRY_DELAY_MS` (2.5s) with [`tokio::time::sleep`].
//!
//! [`Stream::behavior_hints`]: vsources_core::types::Stream::behavior_hints
//! [`HubExtractor`]: super::hubextractor::HubExtractor

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::language::find_country_codes;
use vsources_core::resolution::find_height;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{CountryCode, Format, Stream};

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"hubcloud");

/// Hosts of the whole hub family — `HUB_HOST_PATTERN` from
/// `src/utils/hub.js`, shared with [`HubExtractor`](super::hubextractor::HubExtractor)
/// and [`HBLinks`](super::hblinks::HBLinks).
pub(crate) static HUB_HOST_PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"hubcdn|hubcloud|hubdrive|gdflix|gyanigurus")
        .unwrap_or_else(|e| panic!("valid hub host pattern: {e}"))
});

/// `HubCloud` hosts known to be dead — `DEAD_HUBCLOUD_HOSTS` from
/// `src/utils/hub.js`.
pub(crate) const DEAD_HUBCLOUD_HOSTS: &[&str] = &[
    "hubcloud.ink",
    "hubcloud.co",
    "hubcloud.cc",
    "hubcloud.me",
    "hubcloud.xyz",
];

/// `HubCloud` result lifetime — `HUBCLOUD_CACHE_TTL` (5 minutes) from
/// `src/utils/hub.js`. Short on purpose: hubcloud workers.dev URLs embed
/// session tokens that expire quickly; a 5-minute-old URL 403s.
pub(crate) const HUBCLOUD_CACHE_TTL: Duration = Duration::from_mins(5);

/// Delay before retrying hop 1 after a failed hop 2 (`RETRY_DELAY_MS`).
const RETRY_DELAY_MS: Duration = Duration::from_millis(2500);

/// Metadata carried down the hub chain — upstream's `meta` argument.
///
/// Upstream also threads `meta.referer` (ports to
/// [`ResolveCtx::referer`]) and `meta.title` (cut, see the module docs).
#[derive(Debug, Clone, Default)]
pub(crate) struct HubMeta {
    /// Language tags gathered from page titles along the chain.
    pub(crate) country_codes: Vec<CountryCode>,
    /// Vertical resolution parsed from a page title.
    pub(crate) height: Option<u16>,
    /// File size parsed from a page's size cell.
    pub(crate) size: Option<u64>,
}

/// `{ ...base, ...overlay, countryCodes: union }` — overlay wins for the
/// scalar fields, the language tags merge in order.
pub(crate) fn enrich_meta(base: &HubMeta, overlay: &HubMeta) -> HubMeta {
    let mut country_codes = base.country_codes.clone();
    for code in &overlay.country_codes {
        if !country_codes.contains(code) {
            country_codes.push(*code);
        }
    }
    HubMeta {
        country_codes,
        height: overlay.height.or(base.height),
        size: overlay.size.or(base.size),
    }
}

/// True CDN (`GDrive`) host that would duplicate a `HubCloud` result —
/// upstream `isCdnDirectUrl`.
pub(crate) fn is_cdn_direct_url(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| host.contains("googleusercontent.com"))
}

/// Direct media file URL — Task-41 (OOM fix): these must never be
/// fetched as text (that buffers the whole video in memory) nor
/// delegated to the hubcloud page chain; they are already playable
/// cards.
///
/// Ports upstream `isDirectFileUrl`: r2.dev/r2 hosts, or a media
/// extension on the path.
pub(crate) fn is_direct_file_url(url: &Url) -> bool {
    static R2_HOST: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?i)\.r2\.dev$|(^|\.)r2\.cloudflarestorage\.com$")
            .unwrap_or_else(|e| panic!("valid r2 host pattern: {e}"))
    });
    static MEDIA_EXT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?i)\.(mkv|mp4|avi|webm|mov|m3u8|ts)$")
            .unwrap_or_else(|e| panic!("valid media extension pattern: {e}"))
    });
    let host = url.host_str().unwrap_or_default();
    if R2_HOST.is_match(host).unwrap_or(false) {
        return true;
    }
    MEDIA_EXT.is_match(url.path()).unwrap_or(false)
}

// ─── bytes npm size parsing ────────────────────────────────────────────

/// The `bytes` package's unit table, in exact integer arithmetic.
const SIZE_UNITS: &[(&str, u128)] = &[
    ("b", 1),
    ("kb", 1 << 10),
    ("mb", 1 << 20),
    ("gb", 1 << 30),
    ("tb", 1_u128 << 40),
    ("pb", 1_u128 << 50),
];

static SIZE_LABEL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)^((-|\+)?(\d+(?:\.\d+)?)) *(kb|mb|gb|tb|pb)$")
        .unwrap_or_else(|e| panic!("valid size pattern: {e}"))
});

/// Parse a human size label into bytes — the `bytes` npm package's
/// `parse` (used on the `File Size` cell and the download page's `#size`).
///
/// Semantics, ported exactly: `<number> [kb|mb|gb|tb|pb]` with the unit
/// in powers of two, floored; anything else falls back to JS
/// `parseInt`'s leading-digit read (unit-less bytes); unparseable input
/// yields `None` (upstream `null`). Negative results are dropped — a
/// file size is unsigned here.
pub(crate) fn parse_size_label(text: &str) -> Option<u64> {
    let (number, multiplier) = match SIZE_LABEL.captures(text).ok().flatten() {
        Some(caps) => (caps.get(1)?.as_str().to_string(), unit_of(&caps)),
        None => (leading_int(text)?.to_string(), 1),
    };
    let negative = number.starts_with('-');
    let unsigned = number
        .strip_prefix('-')
        .or_else(|| number.strip_prefix('+'))
        .unwrap_or(&number);
    let (whole, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let whole: u128 = whole.parse().ok()?;
    let total = whole.checked_mul(multiplier)?;
    let extra = if frac.is_empty() {
        0
    } else {
        let digits: u128 = frac.parse().ok()?;
        let scale = 10u128.checked_pow(u32::try_from(frac.len()).ok()?)?;
        multiplier.checked_mul(digits)?.checked_div(scale)?
    };
    let total = total.checked_add(extra)?;
    if negative {
        return None;
    }
    u64::try_from(total).ok()
}

/// The unit multiplier of a matched size label.
fn unit_of(caps: &fancy_regex::Captures<'_, str>) -> u128 {
    let unit = caps.get(4).map_or_else(
        || "b".to_string(),
        |group| group.as_str().to_ascii_lowercase(),
    );
    SIZE_UNITS
        .iter()
        .find(|(name, _)| *name == unit)
        .map_or(1, |(_, multiplier)| *multiplier)
}

/// JS `parseInt`'s leading-digit read: optional sign, then ASCII digits.
fn leading_int(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let after_sign = trimmed.strip_prefix(['-', '+']).unwrap_or(trimmed);
    let digits = after_sign.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let sign = trimmed.len() - after_sign.len();
    Some(&trimmed[..sign + digits])
}

// ─── HubCloud redirect-page strategies ────────────────────────────────

static REDIRECT_VAR_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"var url\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid var url pattern: {e}"))
});
static REDIRECT_WINDOW_LOCATION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"window\.location(?:\.href)?\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid window location pattern: {e}"))
});
static REDIRECT_LOCATION_REPLACE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"location\.replace\(['"](.*?)['"]\)"#)
        .unwrap_or_else(|e| panic!("valid location replace pattern: {e}"))
});
static REDIRECT_META_REFRESH: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r#"(?i)<meta[^>]*http-equiv=["']?refresh["']?[^>]*content=["']?\d+;\s*url=(.*?)["']"#,
    )
    .unwrap_or_else(|e| panic!("valid meta refresh pattern: {e}"))
});
static REDIRECT_DOCUMENT_LOCATION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"document\.location(?:\.href)?\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid document location pattern: {e}"))
});
static REDIRECT_LOCATION_HREF: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"location\.href\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid location href pattern: {e}"))
});
static REDIRECT_LOCATION_ASSIGN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"location\.assign\(['"](.*?)['"]\)"#)
        .unwrap_or_else(|e| panic!("valid location assign pattern: {e}"))
});
static REDIRECT_WINDOW_OPEN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"window\.open\(['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid window open pattern: {e}"))
});
static REDIRECT_DATA_ATTR: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"data-(?:url|href|link)\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid data attribute pattern: {e}"))
});
static REDIRECT_IFRAME: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"<iframe[^>]+src\s*=\s*['"](.*?)['"]"#)
        .unwrap_or_else(|e| panic!("valid iframe pattern: {e}"))
});
static REDIRECT_VAR_HUB: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r#"var\s+\w+\s*=\s*['"]([^'"]*(?:hubcloud|gamerxyt|hubdrive|hubcdn)[^'"]*)['"]"#,
    )
    .unwrap_or_else(|e| panic!("valid hub var pattern: {e}"))
});
static REDIRECT_BRUTE_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r#"https?://(?:hubcloud\.[a-z.]+|hubdrive\.[a-z.]+|gamerxyt\.com|hubcdn)[^\s'"<>)]+"#,
    )
    .unwrap_or_else(|e| panic!("valid brute url pattern: {e}"))
});
static COOKIE_NAME: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"stck\(\s*['"](\w+)['"]\s*,"#)
        .unwrap_or_else(|e| panic!("valid stck pattern: {e}"))
});
static PIXELDRAIN_U: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"pixeldrain\.(?:dev|com)/u/([^?&]+)")
        .unwrap_or_else(|e| panic!("valid pixeldrain pattern: {e}"))
});

/// The first capture of `regex` in `text`, when it is non-empty (an
/// empty capture is falsy in the upstream strategy loop).
pub(crate) fn capture_nonempty(regex: &fancy_regex::Regex, text: &str) -> Option<String> {
    first_capture(regex, text)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
}

// ─── JS href-override patching ─────────────────────────────────────────

static VAR_ASSIGN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"var\s+(\w+)\s*=\s*(["'])([^"'<>]{8,}?)\2"#)
        .unwrap_or_else(|e| panic!("valid var assignment pattern: {e}"))
});
static GET_ELEMENT_HREF: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r#"document\.getElementById\((["'])([^"']+)\1\)\.href\s*=\s*(?:["']([^"'<>]+)["']|([\w$]+))"#,
    )
    .unwrap_or_else(|e| panic!("valid getElementById pattern: {e}"))
});
static ANCHOR_OPEN_TAG: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"<a\b[^>]*>")
        .unwrap_or_else(|e| panic!("valid anchor open tag pattern: {e}"))
});
static ANCHOR_ID: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"\bid=["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid anchor id pattern: {e}"))
});
static HREF_IN_TAG: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(\shref=)(["'])[^"']*\2"#)
        .unwrap_or_else(|e| panic!("valid href pattern: {e}"))
});

/// Rewrite anchor `href`s with the values a page's inline script assigns
/// via `getElementById(...).href = …`.
///
/// `HubCloud` worker pages (2025+) hide the real download URL behind a
/// tiny script: the static `href` points at a dead DMCA honeypot file
/// and the script swaps in the live URL. Server-side parsing (and this
/// port) would otherwise see the dead href. Conservative: without the
/// script the HTML is returned unchanged.
fn apply_js_href_overrides(html: &str) -> String {
    if !html.contains("getElementById") {
        return html.to_string();
    }

    // 1. Simple string assignments: var name = "url".
    let var_map: HashMap<String, String> = VAR_ASSIGN
        .captures_iter(html)
        .filter_map(Result::ok)
        .filter_map(|caps| {
            let name = caps.get(1)?.as_str().to_string();
            let value = caps.get(3)?.as_str().to_string();
            Some((name, value))
        })
        .collect();

    // 2. getElementById("id").href = "literal" | varName (a JS Map: a
    // repeated id keeps the last assignment).
    let mut id_to_url: Vec<(String, String)> = Vec::new();
    for caps in GET_ELEMENT_HREF.captures_iter(html).filter_map(Result::ok) {
        let Some(id) = caps.get(2).map(|group| group.as_str().to_string()) else {
            continue;
        };
        let value = caps
            .get(3)
            .map(|group| group.as_str().to_string())
            .or_else(|| {
                caps.get(4)
                    .and_then(|group| var_map.get(group.as_str()).cloned())
            });
        if let Some(value) = value
            && (value.starts_with("http://")
                || value.starts_with("https://")
                || value.starts_with('/'))
        {
            id_to_url.retain(|(existing, _)| *existing != id);
            id_to_url.push((id, value));
        }
    }
    if id_to_url.is_empty() {
        return html.to_string();
    }

    // 3. Rewrite the matching anchor's href (the id attribute may
    // precede or follow href).
    let mut out = html.to_string();
    for (id, url) in id_to_url {
        let tags: Vec<String> = ANCHOR_OPEN_TAG
            .captures_iter(&out)
            .filter_map(Result::ok)
            .filter_map(|caps| caps.get(0).map(|group| group.as_str().to_string()))
            .collect();
        let Some(tag) = tags
            .into_iter()
            .find(|tag| anchor_id(tag).as_deref() == Some(id.as_str()))
        else {
            continue;
        };
        if let Some(patched) = patch_anchor_href(&tag, &url) {
            out = out.replacen(&tag, &patched, 1);
        }
    }
    out
}

/// The `id` attribute of an anchor open tag.
fn anchor_id(tag: &str) -> Option<String> {
    first_capture(&ANCHOR_ID, tag).ok().flatten()
}

/// `<a … href="dead">` with the href value swapped for `url`.
fn patch_anchor_href(tag: &str, url: &str) -> Option<String> {
    let caps = HREF_IN_TAG.captures(tag).ok().flatten()?;
    let whole = caps.get(0)?;
    let lead = caps.get(1)?;
    let quote = caps.get(2)?;
    let mut patched = String::with_capacity(tag.len() + url.len());
    patched.push_str(&tag[..whole.start()]);
    patched.push_str(lead.as_str());
    patched.push_str(quote.as_str());
    patched.push_str(url);
    patched.push_str(quote.as_str());
    patched.push_str(&tag[whole.end()..]);
    Some(patched)
}

// ─── Download-page classification ─────────────────────────────────────

/// What a category does with the matched anchor's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonTransform {
    /// Keep the href as written.
    None,
    /// `PixelServer : 2`: the pixeldrain `/u/{id}` viewer page becomes
    /// `/api/file/{id}?download` (the `/u/` page is HTML, `/api/file/`
    /// returns the raw video).
    PixelDrain,
}

/// One download-button category — upstream `SERVER_CATEGORIES`. The
/// upstream `priority` field is cut (declared but never read).
#[derive(Debug)]
struct ServerCategory {
    /// Substring the anchor text must contain.
    button_includes: &'static str,
    /// Substring the anchor text must not contain (empty = no exclusion).
    button_excludes: &'static str,
    /// The stream label.
    label: &'static str,
    /// The upstream `meta.extractorId` — carried as the `bingeGroup`
    /// behavior hint.
    extractor_id: &'static str,
    /// Whether the mirror supports HTTP Range seeking.
    seekable: bool,
    /// The URL transform, if any.
    transform: ButtonTransform,
}

static SERVER_CATEGORIES: &[ServerCategory] = &[
    ServerCategory {
        button_includes: "FSLv2",
        button_excludes: "",
        label: "HubCloud (FSLv2)",
        extractor_id: "hubcloud_fslv2",
        seekable: true,
        transform: ButtonTransform::None,
    },
    ServerCategory {
        button_includes: "FSL",
        button_excludes: "FSLv2",
        label: "HubCloud (FSL)",
        extractor_id: "hubcloud_fsl",
        seekable: true,
        transform: ButtonTransform::None,
    },
    ServerCategory {
        button_includes: "10Gbps",
        button_excludes: "",
        label: "HubCloud (10Gbps)",
        extractor_id: "hubcloud_fast",
        seekable: true,
        transform: ButtonTransform::None,
    },
    // PixelServer : 2 must come BEFORE PixelServer — otherwise
    // 'PixelServer' matches first and 'PixelServer : 2' (which links to
    // pixeldrain.dev) is never tested.
    ServerCategory {
        button_includes: "PixelServer : 2",
        button_excludes: "",
        label: "HubCloud (PixelDrain)",
        extractor_id: "hubcloud_pixeldrain",
        seekable: true,
        transform: ButtonTransform::PixelDrain,
    },
    ServerCategory {
        button_includes: "PixelServer",
        button_excludes: "",
        label: "HubCloud (PxlSrv)",
        extractor_id: "hubcloud_pixelserver",
        seekable: true,
        transform: ButtonTransform::None,
    },
    ServerCategory {
        button_includes: "PDL",
        button_excludes: "",
        label: "HubCloud (PDL)",
        extractor_id: "hubcloud_pdl",
        seekable: false,
        transform: ButtonTransform::None,
    },
    ServerCategory {
        button_includes: "Download File",
        button_excludes: "",
        label: "HubCloud (Download)",
        extractor_id: "hubcloud_direct",
        seekable: true,
        transform: ButtonTransform::None,
    },
];

static SEL_ANCHOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a").unwrap_or_else(|e| panic!("valid anchor selector: {e}")));
static SEL_TITLE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("title").unwrap_or_else(|e| panic!("valid title selector: {e}"))
});
static SEL_SIZE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("#size").unwrap_or_else(|e| panic!("valid size selector: {e}"))
});

/// The extended download-content selectors of
/// `hasValidDownloadContent`.
static EXTENDED_SELECTORS: [LazyLock<Selector>; 11] = [
    LazyLock::new(|| {
        Selector::parse("a#download").unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="hubcloud.php"]"#)
            .unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="gamerxyt.com"]"#)
            .unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="hubcloud.one"]"#)
            .unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="workers.dev"]"#)
            .unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="hubcdn"]"#).unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(".download-btn").unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(r#"a[href*="download"]"#).unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse("a.btn.btn-primary").unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(".btn-success").unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
    LazyLock::new(|| {
        Selector::parse(".btn-danger").unwrap_or_else(|e| panic!("valid selector: {e}"))
    }),
];

/// The `HubCloud` family extractor: the page behind a hubcloud link.
///
/// Stateless — every hop travels through the resolve context's fetcher,
/// so the only state upstream owns (the cookie jar) lives on the
/// fetcher, not here.
#[derive(Debug, Default)]
pub struct HubCloud;

impl HubCloud {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl HubCloud {
    /// Ports `extractInternal` — the shared entry point the other hub
    /// extractors call with chain metadata.
    pub(crate) async fn extract_internal(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
        meta: &HubMeta,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Task-41 (OOM fix): a caller handing us a DIRECT FILE URL gets
        // the playable card instead of a fatal fetch-as-text download.
        if is_direct_file_url(url) {
            let hls = url.path().to_ascii_lowercase().ends_with(".m3u8");
            let mut stream = Stream::new(url.clone(), if hls { Format::Hls } else { Format::Mp4 })
                .with_ttl(HUBCLOUD_CACHE_TTL)
                .with_label("HubCloud (Direct)");
            apply_meta(&mut stream, meta);
            stream
                .behavior_hints
                .insert("bingeGroup".to_string(), "hubcloud_directfile".to_string());
            return Ok(vec![stream]);
        }

        let referer = ctx.referer.unwrap_or(url);
        let redirect_html = fetch_page_with(ctx, url, referer).await?;
        let Some(raw_redirect_url) = extract_redirect_url(&redirect_html) else {
            return Err(ExtractorError::NotFound);
        };
        let redirect_url = resolve_redirect_target(url, &raw_redirect_url)?;
        let cookie_name = extract_cookie_name(&redirect_html);

        let Some(links_html) =
            download_page_with_retry(ctx, url, referer, &redirect_url, cookie_name.as_deref())
                .await?
        else {
            return Err(ExtractorError::NotFound);
        };
        // Parse once, then drop the document: it is not `Send`, and the
        // classification loop below crosses an await (the PixelServer
        // liveness HEAD probe).
        let (title, size, anchors) = {
            let document = Html::parse_document(&links_html);
            (
                element_text(&document, &SEL_TITLE),
                parse_size_label(&element_text(&document, &SEL_SIZE)),
                anchor_pairs(&document),
            )
        };

        let title = title.trim().to_string();
        let mut country_codes = meta.country_codes.clone();
        for code in find_country_codes(&title) {
            if !country_codes.contains(&code) {
                country_codes.push(code);
            }
        }
        let height = meta.height.or_else(|| find_height(&title));
        let classified = classify_buttons(ctx, &anchors, &country_codes, height, size).await?;

        // Drop non-seekable results that have a seekable sibling for the
        // same file (unknown size counts as a match, like upstream's
        // undefined === undefined).
        let seekable_sizes: Vec<Option<u64>> = classified
            .iter()
            .filter(|(_, seekable)| *seekable)
            .map(|(stream, _)| stream.meta.size)
            .collect();
        let streams: Vec<Stream> = if seekable_sizes.is_empty() {
            classified.into_iter().map(|(stream, _)| stream).collect()
        } else {
            classified
                .into_iter()
                .filter(|(stream, seekable)| {
                    *seekable || !seekable_sizes.contains(&stream.meta.size)
                })
                .map(|(stream, _)| stream)
                .collect()
        };
        // An empty classification is upstream's `[]` — a miss.
        if streams.is_empty() {
            return Err(ExtractorError::NotFound);
        }
        Ok(streams)
    }
}

/// The download-button classification loop (upstream's loop over
/// `SERVER_CATEGORIES`): one (stream, seekable) pair per matching
/// anchor, in category order — the upstream `LABEL_TO_SEEKABLE` flag
/// carried alongside.
async fn classify_buttons(
    ctx: &ResolveCtx<'_>,
    anchors: &[(String, Option<String>)],
    country_codes: &[CountryCode],
    height: Option<u16>,
    size: Option<u64>,
) -> Result<Vec<(Stream, bool)>, ExtractorError> {
    let mut matched = vec![false; anchors.len()];
    let mut classified: Vec<(Stream, bool)> = Vec::new();

    for category in SERVER_CATEGORIES {
        for (index, (text, href)) in anchors.iter().enumerate() {
            if matched[index] {
                continue;
            }
            let Some(href) = href.as_deref() else {
                continue;
            };
            if href.to_lowercase().contains(".zip") {
                continue;
            }
            if !text.contains(category.button_includes) {
                continue;
            }
            if !category.button_excludes.is_empty() && text.contains(category.button_excludes) {
                continue;
            }
            matched[index] = true;

            if category.button_includes == "PixelServer" {
                // PixelServer: the /api/file/ link is HEAD-checked — a
                // dead mirror is skipped, a live one ships with the /u/
                // viewer page as its Referer.
                let Ok(user_url) = Url::parse(&href.replacen("/api/file/", "/u/", 1)) else {
                    continue;
                };
                let Ok(mut api_url) =
                    Url::parse(&user_url.as_str().replacen("/u/", "/api/file/", 1))
                else {
                    continue;
                };
                set_download_param(&mut api_url);
                let head = FetchRequest::head(api_url.clone())
                    .with_header("Referer", user_url.to_string());
                if ctx.fetcher.request(head).await.is_err() {
                    continue;
                }
                let mut stream = Stream::new(api_url, Format::Unknown)
                    .with_ttl(HUBCLOUD_CACHE_TTL)
                    .with_label(category.label);
                stream.meta.languages = country_codes.to_vec();
                stream.meta.resolution = height;
                stream.meta.size = size;
                stream
                    .meta
                    .request_headers
                    .insert("Referer".to_string(), user_url.to_string());
                stream
                    .behavior_hints
                    .insert("bingeGroup".to_string(), category.extractor_id.to_string());
                classified.push((stream, category.seekable));
            } else {
                let final_url = match category.transform {
                    ButtonTransform::PixelDrain => transform_pixeldrain(href),
                    ButtonTransform::None => href.to_string(),
                };
                let url = Url::parse(&final_url).map_err(|error| {
                    ExtractorError::extraction(
                        "hubcloud",
                        format!("unusable link {final_url}: {error}"),
                    )
                })?;
                let mut stream = Stream::new(url, Format::Unknown)
                    .with_ttl(HUBCLOUD_CACHE_TTL)
                    .with_label(category.label);
                stream.meta.languages = country_codes.to_vec();
                stream.meta.resolution = height;
                stream.meta.size = size;
                stream
                    .behavior_hints
                    .insert("bingeGroup".to_string(), category.extractor_id.to_string());
                if !category.seekable {
                    // Upstream appends `⚠️ no seek` to the (cut) meta
                    // title — the marker survives as a hint.
                    stream
                        .behavior_hints
                        .insert("noSeek".to_string(), "true".to_string());
                }
                classified.push((stream, category.seekable));
            }
        }
    }
    Ok(classified)
}

/// Hop 2, with the upstream retry: when the worker serves a placeholder
/// page, wait out the rotation window, re-run hop 1, and fetch the
/// (possibly different) download page again. `None` when the page still
/// has no download content.
async fn download_page_with_retry(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &Url,
    redirect_url: &Url,
    cookie_name: Option<&str>,
) -> Result<Option<String>, ExtractorError> {
    let links_html = fetch_download_page(ctx, redirect_url, url, cookie_name).await?;
    let links_html = apply_js_href_overrides(&links_html);
    if has_valid_download_content(&Html::parse_document(&links_html)) {
        return Ok(Some(links_html));
    }

    tokio::time::sleep(RETRY_DELAY_MS).await;
    let retry_html = fetch_page_with(ctx, url, referer).await?;
    if let Some(raw_retry_redirect_url) = extract_redirect_url(&retry_html) {
        let retry_redirect_url = resolve_redirect_target(url, &raw_retry_redirect_url)?;
        let retry_cookie_name = extract_cookie_name(&retry_html);
        let retry_links_html =
            fetch_download_page(ctx, &retry_redirect_url, url, retry_cookie_name.as_deref())
                .await?;
        let retry_links_html = apply_js_href_overrides(&retry_links_html);
        if has_valid_download_content(&Html::parse_document(&retry_links_html)) {
            return Ok(Some(retry_links_html));
        }
    }
    Ok(None)
}

/// Apply `HubMeta` onto a stream (upstream spreads `meta` into every
/// result).
fn apply_meta(stream: &mut Stream, meta: &HubMeta) {
    stream.meta.languages.clone_from(&meta.country_codes);
    stream.meta.resolution = meta.height;
    stream.meta.size = meta.size;
}

/// `searchParams.set('download', '')` — replace or append.
fn set_download_param(url: &mut Url) {
    let existing: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut serializer = url.query_pairs_mut();
    serializer.clear();
    for (name, value) in existing {
        if name != "download" {
            serializer.append_pair(&name, &value);
        }
    }
    serializer.append_pair("download", "");
}

/// pixeldrain `/u/{id}` → `/api/file/{id}?download`.
fn transform_pixeldrain(url: &str) -> String {
    match capture_nonempty(&PIXELDRAIN_U, url) {
        Some(id) => format!("https://pixeldrain.dev/api/file/{id}?download"),
        None => url.to_string(),
    }
}

/// The first working redirect URL out of the upstream strategy list, in
/// order. Empty captures are skipped (falsy upstream).
fn extract_redirect_url(html: &str) -> Option<String> {
    if let Some(value) = capture_nonempty(&REDIRECT_VAR_URL, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_WINDOW_LOCATION, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_LOCATION_REPLACE, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_META_REFRESH, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_DOCUMENT_LOCATION, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_LOCATION_HREF, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_LOCATION_ASSIGN, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_WINDOW_OPEN, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_DATA_ATTR, html) {
        return Some(value);
    }
    if let Some(src) = capture_nonempty(&REDIRECT_IFRAME, html)
        && (src.contains("hubcloud") || src.contains("gamerxyt"))
    {
        return Some(src);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_VAR_HUB, html) {
        return Some(value);
    }
    if let Some(value) = capture_nonempty(&REDIRECT_BRUTE_URL, html) {
        tracing::warn!(
            "brute-force URL extraction used — redirect strategy array may need updating. Extracted: {value}"
        );
        return Some(value);
    }
    None
}

/// The hop-2 cookie name from the redirect page's `stck` call.
fn extract_cookie_name(html: &str) -> Option<String> {
    capture_nonempty(&COOKIE_NAME, html)
}

/// `raw.startsWith('http') ? raw : url.origin + raw`, parsed.
fn resolve_redirect_target(url: &Url, raw: &str) -> Result<Url, ExtractorError> {
    let target = if raw.starts_with("http") {
        raw.to_string()
    } else {
        format!("{}{raw}", url.origin().ascii_serialization())
    };
    Url::parse(&target).map_err(|error| {
        ExtractorError::extraction(
            "hubcloud",
            format!("unusable redirect target {target}: {error}"),
        )
    })
}

/// Fetch the download page with the hubcloud link as Referer and the
/// `stck` cookie attached (upstream plants it in the fetcher's jar).
async fn fetch_download_page(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &Url,
    cookie_name: Option<&str>,
) -> Result<String, ExtractorError> {
    let mut request = FetchRequest::get(url.clone()).with_header("Referer", referer.to_string());
    if let Some(cookie_name) = cookie_name {
        request = request.with_header("Cookie", format!("{cookie_name}=s4t"));
    }
    Ok(ctx.fetcher.request(request).await?.body)
}

/// Whether the page looks like a real download page — ports
/// `hasValidDownloadContent`.
fn has_valid_download_content(document: &Html) -> bool {
    if document.select(&SEL_SIZE).next().is_some() {
        return true;
    }
    for element in document.select(&SEL_ANCHOR) {
        let text: String = element.text().collect();
        if text.contains("FSL") || text.contains("PixelServer") {
            return true;
        }
    }
    EXTENDED_SELECTORS
        .iter()
        .any(|selector| document.select(selector).next().is_some())
}

pub(crate) fn element_text(document: &Html, selector: &Selector) -> String {
    document
        .select(selector)
        .next()
        .map(|element| element.text().collect::<String>())
        .unwrap_or_default()
}

/// The `<title>` text of a document — `$('title').text()`.
pub(crate) fn title_text(document: &Html) -> String {
    element_text(document, &SEL_TITLE)
}

/// The anchors (`<a>`) of a document as (text, href) pairs, in document
/// order — `$('a').toArray()`.
pub(crate) fn anchor_pairs(document: &Html) -> Vec<(String, Option<String>)> {
    document
        .select(&SEL_ANCHOR)
        .map(|element| {
            (
                element.text().collect::<String>(),
                element.attr("href").map(str::to_string),
            )
        })
        .collect()
}

#[async_trait]
impl Extractor for HubCloud {
    fn id(&self) -> &'static str {
        "hubcloud"
    }

    fn label(&self) -> &'static str {
        "HubCloud"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    fn cache_version(&self) -> Option<u32> {
        Some(13)
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

    fn url() -> Url {
        Url::parse("https://hubcloud.one/file/xyz").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_hubcloud_hosts() {
        let extractor = HubCloud::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/file/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("hubcloud.one"));
        assert!(supports("hubcloud.ist"));
        assert!(!supports("hubcdn.club"));
        assert!(!supports("hubdrive.pics"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn reports_the_upstream_cache_version() {
        assert_eq!(HubCloud::new().cache_version(), Some(13));
    }

    #[test]
    fn detects_direct_file_urls() {
        let parse = |value: &str| Url::parse(value).unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert!(is_direct_file_url(&parse("https://file.r2.dev/video.mkv")));
        assert!(is_direct_file_url(&parse(
            "https://pub.r2.cloudflarestorage.com/video"
        )));
        assert!(is_direct_file_url(&parse(
            "https://cdn.example.com/movie.mp4"
        )));
        assert!(is_direct_file_url(&parse(
            "https://cdn.example.com/hls/index.m3u8"
        )));
        assert!(is_direct_file_url(&parse(
            "https://cdn.example.com/segment.ts"
        )));
        assert!(!is_direct_file_url(&parse("https://hubcloud.one/file/xyz")));
        assert!(!is_direct_file_url(&parse(
            "https://cdn.example.com/video.mp4x"
        )));
    }

    #[test]
    fn parses_size_labels_like_the_bytes_package() {
        // 1.4 GB = floor(1.4 × 2^30) — powers of two, floored.
        assert_eq!(parse_size_label("1.4 GB"), Some(1_503_238_553));
        assert_eq!(parse_size_label("2.1GB"), Some(2_254_857_830));
        assert_eq!(parse_size_label("1.5 MB"), Some(1_572_864));
        assert_eq!(parse_size_label("1kb"), Some(1024));
        // Unit-less values are plain bytes, parseInt-style.
        assert_eq!(parse_size_label("1024"), Some(1024));
        assert_eq!(parse_size_label("300 B"), Some(300));
        // Unparseable input is null upstream.
        assert_eq!(parse_size_label(""), None);
        assert_eq!(parse_size_label("abc"), None);
        assert_eq!(parse_size_label("1.4 XB"), Some(1));
    }

    #[test]
    fn rewrites_scripted_anchor_hrefs() {
        let html = concat!(
            r#"<a id="pxl-1" href="https://pixeldrain.dev/u/DEADID">Download [PixelServer : 2]</a>"#,
            r#"<script> var pxl = "https://pixeldrain.dev/u/LIVEID";"#,
            r#"document.getElementById("pxl-1").href = pxl; </script>"#,
        );
        let patched = apply_js_href_overrides(html);
        assert!(patched.contains("https://pixeldrain.dev/u/LIVEID"));
        assert!(!patched.contains("DEADID"));
        // Without the script, the HTML is untouched.
        let plain = r#"<a href="https://x.dev/a">a</a>"#;
        assert_eq!(apply_js_href_overrides(plain), plain);
    }

    #[tokio::test]
    async fn ships_direct_file_urls_without_fetching() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let direct = Url::parse("https://file.r2.dev/video.mkv")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = HubCloud::new()
            .extract(&ctx, &direct)
            .await
            .unwrap_or_else(|e| panic!("the direct file card must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, Format::Mp4);
        assert_eq!(streams[0].url.as_str(), "https://file.r2.dev/video.mkv");
        assert_eq!(streams[0].label.as_deref(), Some("HubCloud (Direct)"));
        assert_eq!(streams[0].ttl, HUBCLOUD_CACHE_TTL);
        assert_eq!(
            streams[0]
                .behavior_hints
                .get("bingeGroup")
                .map(String::as_str),
            Some("hubcloud_directfile")
        );
        assert!(
            fetcher.requests().is_empty(),
            "the file must never be fetched"
        );
    }

    #[tokio::test]
    async fn classifies_the_download_page_buttons() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/file/xyz",
                r#"<html><head><title>HubCloud</title></head><body><script>stck('dls', 1); var url = "/dl/xyz";</script></body></html>"#,
            )
            .page(
                "/dl/xyz",
                concat!(
                    r#"<html><head><title>Movie 1080p English</title></head><body>"#,
                    r#"<div id="size">1.4 GB</div>"#,
                    r#"<a href="https://hubcloud.one/workers/file1">Download File</a>"#,
                    r#"<a href="https://hubcloud.one/pdl/1">PDL</a>"#,
                    r#"</body></html>"#,
                ),
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HubCloud::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the download page must resolve: {e}"));
        // PDL is dropped: a seekable sibling carries the same size.
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("HubCloud (Download)"));
        assert_eq!(stream.url.as_str(), "https://hubcloud.one/workers/file1");
        assert_eq!(stream.format, Format::Unknown);
        assert_eq!(stream.ttl, HUBCLOUD_CACHE_TTL);
        assert_eq!(stream.meta.size, Some(1_503_238_553));
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.languages, vec![CountryCode::En]);
        assert_eq!(
            stream.behavior_hints.get("bingeGroup").map(String::as_str),
            Some("hubcloud_direct")
        );

        // Hop 1 carried the hubcloud URL as Referer; hop 2 carried it
        // again plus the stck cookie (upstream's jar plant).
        assert_eq!(
            fetcher.sent_header("/file/xyz", "Referer").as_deref(),
            Some("https://hubcloud.one/file/xyz")
        );
        assert_eq!(
            fetcher.sent_header("/dl/xyz", "Referer").as_deref(),
            Some("https://hubcloud.one/file/xyz")
        );
        assert_eq!(
            fetcher.sent_header("/dl/xyz", "Cookie").as_deref(),
            Some("dls=s4t")
        );
    }

    #[tokio::test]
    async fn keeps_a_lone_unseekable_button_with_the_marker() {
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                r#"<html><head><title>Movie</title></head><body><a href="https://hubcloud.one/pdl/1">PDL</a></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HubCloud::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the lone PDL button must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("HubCloud (PDL)"));
        assert_eq!(
            streams[0].behavior_hints.get("noSeek").map(String::as_str),
            Some("true")
        );
        assert_eq!(streams[0].meta.size, None);
    }

    #[tokio::test]
    async fn zip_links_are_skipped() {
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                r#"<html><head><title>Movie</title></head><body><a href="https://hubcloud.one/pack.zip">Download File</a></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        match HubCloud::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a zip-only page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pixelserver_links_are_head_checked_and_rewritten() {
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                concat!(
                    r#"<html><head><title>Movie</title></head><body>"#,
                    r#"<a href="https://pixeldrain.dev/api/file/abc123">PixelServer</a>"#,
                    r#"</body></html>"#,
                ),
            )
            .page("/api/file/abc123", "");
        let ctx = ctx_for(&fetcher, None);

        let streams = HubCloud::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the pixelserver link must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("HubCloud (PxlSrv)"));
        assert_eq!(
            stream.url.as_str(),
            "https://pixeldrain.dev/api/file/abc123?download="
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://pixeldrain.dev/u/abc123")
        );
        // The liveness HEAD probe carried the viewer page as Referer.
        assert_eq!(
            fetcher
                .sent_header("/api/file/abc123", "Referer")
                .as_deref(),
            Some("https://pixeldrain.dev/u/abc123")
        );
    }

    #[tokio::test]
    async fn dead_pixelserver_links_are_skipped() {
        // No page for /api/file/dead → the HEAD probe fails → no stream.
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                r#"<html><head><title>Movie</title></head><body><a href="https://pixeldrain.dev/api/file/dead">PixelServer</a></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        match HubCloud::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a dead pixelserver link must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pixeldrain_viewer_pages_become_api_downloads() {
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                r#"<html><head><title>Movie</title></head><body><a href="https://pixeldrain.dev/u/xyz789">PixelServer : 2</a></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HubCloud::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the pixeldrain link must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("HubCloud (PixelDrain)"));
        assert_eq!(
            streams[0].url.as_str(),
            "https://pixeldrain.dev/api/file/xyz789?download"
        );
    }

    #[tokio::test]
    async fn scripted_hrefs_feed_the_categories() {
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                concat!(
                    r#"<html><head><title>Movie</title></head><body>"#,
                    r#"<a id="dl-1" href="https://hubcloud.one/dead">Download File</a>"#,
                    r#"<script> var target = "https://hubcloud.one/live/file9";"#,
                    r#"document.getElementById('dl-1').href = target; </script>"#,
                    r#"</body></html>"#,
                ),
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HubCloud::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the scripted href must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), "https://hubcloud.one/live/file9");
    }

    #[tokio::test]
    async fn a_page_without_a_redirect_is_a_miss() {
        let fetcher = ScriptedFetcher::default().page("/file/xyz", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match HubCloud::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a redirect-less page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hop_one_fetch_failures_propagate() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        match HubCloud::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Fetch(_)) => {}
            other => panic!("a failed hop-1 fetch must propagate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_placeholder_download_page_retries_then_misses() {
        // The download page never becomes valid: hop 1 runs twice (the
        // retry) and hop 2 runs twice before giving up.
        let fetcher = ScriptedFetcher::default()
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page("/dl/xyz", "<p>checking your browser</p>");
        let ctx = ctx_for(&fetcher, None);

        let started = std::time::Instant::now();
        match HubCloud::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an invalid download page must be a NotFound, got {other:?}"),
        }
        assert!(
            started.elapsed() >= RETRY_DELAY_MS,
            "the upstream retry delay must be honored"
        );
        let fetch_count = |path: &str| {
            fetcher
                .requests()
                .iter()
                .filter(|request| request.url.path() == path)
                .count()
        };
        assert_eq!(fetch_count("/file/xyz"), 2);
        assert_eq!(fetch_count("/dl/xyz"), 2);
    }

    #[tokio::test]
    async fn redirect_strategies_are_tried_in_order() {
        let extract = extract_redirect_url;
        assert_eq!(
            extract(r#"<script>var url = "/dl/a";</script>"#),
            Some("/dl/a".to_string())
        );
        assert_eq!(
            extract(r#"<script>window.location.href = "/dl/b";</script>"#),
            Some("/dl/b".to_string())
        );
        assert_eq!(
            extract(r"<script>location.replace('/dl/c');</script>"),
            Some("/dl/c".to_string())
        );
        assert_eq!(
            extract(r#"<meta http-equiv="refresh" content="0; url=/dl/d">"#),
            Some("/dl/d".to_string())
        );
        // The iframe strategy only accepts hubcloud/gamerxyt frames.
        assert_eq!(
            extract(r#"<iframe src="https://gamerxyt.com/e/1"></iframe>"#),
            Some("https://gamerxyt.com/e/1".to_string())
        );
        assert_eq!(
            extract(r#"<iframe src="https://example.com/e/1"></iframe>"#),
            None
        );
        assert_eq!(extract("<p>nothing</p>"), None);
    }
}
