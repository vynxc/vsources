//! Cloudflare challenge detection from HTTP responses.
//!
//! Consolidates the detection heuristics of the ported stack: the
//! `cf-mitigated` header and 403 fallback from `src/utils/Fetcher.js`,
//! the challenge-body markers from `QuickNovel`'s `CloudflareKiller`, and
//! the geo/IP-block page shapes the PhoeniX-family sources run into.

use vsources_core::{BlockedReason, FetchResponse};

/// Body markers of a solvable challenge interstitial.
const INTERSTITIAL_MARKERS: &[&str] = &[
    "cf-browser-verification",
    "checking your browser",
    "just a moment",
    "one moment...",
    "un momento…",
    "cf-chl",
    "_cf_chl_opt",
];

/// Body markers of a hard client-IP block.
const BLOCKED_IP_MARKERS: &[&str] = &[
    "sorry, you have been blocked",
    "your ip has been blocked",
    "attention required! | cloudflare",
];

/// Body markers of a censorship/geo block.
const CENSOR_MARKERS: &[&str] = &[
    "not available in your country",
    "page isn’t available in your country",
    "page isn't available in your country",
];

/// The kind of Cloudflare interference a response shows.
///
/// Only [`Challenge::Interstitial`] yields to a clearance cookie; the
/// other variants exist so callers can stop retrying and surface the
/// right error instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Challenge {
    /// A JavaScript/managed interstitial ("Just a moment…"): solvable by
    /// replaying the earned `cf_clearance` cookie.
    Interstitial,
    /// Content withheld by a Cloudflare censorship or geo rule.
    Censor,
    /// The client IP is blocked outright.
    BlockedIp,
}

impl Challenge {
    /// The core [`BlockedReason`] for this challenge kind.
    #[must_use]
    pub fn blocked_reason(self) -> BlockedReason {
        match self {
            Self::Interstitial => BlockedReason::CloudflareChallenge,
            Self::Censor => BlockedReason::CloudflareCensor,
            Self::BlockedIp => BlockedReason::Unknown,
        }
    }

    /// Whether a clearance cookie can resolve this challenge.
    #[must_use]
    pub fn is_solvable(self) -> bool {
        matches!(self, Self::Interstitial)
    }
}

/// Detect a Cloudflare challenge in `response`.
///
/// Evidence is checked cheapest first: the `cf-mitigated` header, a
/// redirect into `/cdn-cgi/`, then body markers (only scanned when the
/// status or headers already suggest Cloudflare, so clean 200s stay free).
/// Returns `None` when nothing indicates Cloudflare interference.
#[must_use]
pub fn detect(response: &FetchResponse) -> Option<Challenge> {
    // The explicit mitigation header is authoritative.
    if response
        .header("cf-mitigated")
        .is_some_and(|value| value.eq_ignore_ascii_case("challenge"))
    {
        return Some(Challenge::Interstitial);
    }

    // A redirect into Cloudflare's challenge endpoint.
    if response
        .header("location")
        .is_some_and(|location| location.to_ascii_lowercase().contains("/cdn-cgi/"))
    {
        return Some(Challenge::Interstitial);
    }

    let cloudflare_served = is_cloudflare_response(response);
    let suspicious_status = matches!(response.status, 403 | 429 | 451 | 503);
    if !cloudflare_served && !suspicious_status {
        return None;
    }

    let body = response.body.to_ascii_lowercase();
    if CENSOR_MARKERS.iter().any(|marker| body.contains(marker)) {
        return Some(Challenge::Censor);
    }
    if BLOCKED_IP_MARKERS
        .iter()
        .any(|marker| body.contains(marker))
    {
        return Some(Challenge::BlockedIp);
    }
    if INTERSTITIAL_MARKERS
        .iter()
        .any(|marker| body.contains(marker))
    {
        return Some(Challenge::Interstitial);
    }

    // A Cloudflare-served error status with no more specific marker is
    // treated as a challenge (mirrors the Fetcher.js 403 fallback).
    if cloudflare_served && matches!(response.status, 403 | 503) {
        return Some(Challenge::Interstitial);
    }
    None
}

/// Whether the response carries Cloudflare fingerprints.
fn is_cloudflare_response(response: &FetchResponse) -> bool {
    response
        .header("server")
        .is_some_and(|server| server.to_ascii_lowercase().contains("cloudflare"))
        || response.header("cf-ray").is_some()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use url::Url;
    use vsources_core::traits::FetchResponse;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> FetchResponse {
        let headers: BTreeMap<String, String> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        FetchResponse {
            url: Url::parse("https://example.com/page")
                .unwrap_or_else(|_| panic!("the URL is valid")),
            status,
            headers,
            body: body.to_string(),
        }
    }

    #[test]
    fn clean_responses_are_ignored() {
        assert_eq!(
            detect(&response(200, &[("server", "nginx")], "<html>ok</html>")),
            None
        );
        assert_eq!(detect(&response(404, &[], "not found")), None);
        // A 403 from a non-Cloudflare server with no markers is not ours.
        assert_eq!(
            detect(&response(403, &[("server", "apache")], "denied")),
            None
        );
    }

    #[test]
    fn passive_cloudflare_javascript_on_real_pages_is_not_a_challenge() {
        // Captured shape from healthy AllWish and FlixCloud pages. Cloudflare
        // injects passive JS detection into ordinary 200 pages too; treating
        // its shared path as a challenge blocked these hosts for an hour.
        let body = r#"<html><title>Watch Frieren</title><main data-id="6351">Player</main>
            <script>var a=document.createElement('script');
            a.src='/cdn-cgi/challenge-platform/scripts/jsd/main.js';</script></html>"#;
        assert_eq!(
            detect(&response(
                200,
                &[("server", "cloudflare"), ("cf-ray", "test")],
                body
            )),
            None
        );
        assert_eq!(
            detect(&response(
                429,
                &[("server", "cloudflare")],
                "Too many requests"
            )),
            None
        );
        assert_eq!(
            detect(&response(
                200,
                &[("server", "cloudflare")],
                "<script>window._cf_chl_opt={}</script>"
            )),
            Some(Challenge::Interstitial)
        );
    }

    #[test]
    fn detects_the_mitigation_header() {
        let subject = response(
            403,
            &[("cf-mitigated", "challenge"), ("server", "cloudflare")],
            "",
        );
        assert_eq!(detect(&subject), Some(Challenge::Interstitial));
        // Case-insensitive value.
        let subject = response(200, &[("cf-mitigated", "Challenge")], "page");
        assert_eq!(detect(&subject), Some(Challenge::Interstitial));
        // A different mitigation value claims nothing.
        let subject = response(200, &[("cf-mitigated", "captcha")], "page");
        assert_eq!(detect(&subject), None);
    }

    #[test]
    fn detects_cdn_cgi_redirects() {
        let subject = response(
            302,
            &[(
                "location",
                "https://example.com/cdn-cgi/challenge-platform/x",
            )],
            "",
        );
        assert_eq!(detect(&subject), Some(Challenge::Interstitial));
    }

    #[test]
    fn detects_interstitial_bodies() {
        let body = "<title>Just a moment...</title><html>checking your browser</html>";
        let subject = response(503, &[("server", "cloudflare")], body);
        assert_eq!(detect(&subject), Some(Challenge::Interstitial));
    }

    #[test]
    fn detects_ip_blocks() {
        let body = "<h1>Sorry, you have been blocked</h1>";
        let subject = response(403, &[("server", "cloudflare"), ("cf-ray", "abc123")], body);
        assert_eq!(detect(&subject), Some(Challenge::BlockedIp));
        assert!(!detect(&subject).is_some_and(Challenge::is_solvable));
    }

    #[test]
    fn detects_censor_blocks() {
        let body = "This page is not available in your country.";
        let subject = response(451, &[("server", "cloudflare")], body);
        assert_eq!(detect(&subject), Some(Challenge::Censor));
        assert_eq!(
            detect(&subject).map(Challenge::blocked_reason),
            Some(BlockedReason::CloudflareCensor)
        );
    }

    #[test]
    fn cloudflare_error_status_without_markers_is_a_challenge() {
        let subject = response(403, &[("server", "cloudflare")], "");
        assert_eq!(detect(&subject), Some(Challenge::Interstitial));
    }
}
