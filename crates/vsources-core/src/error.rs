//! Error taxonomy for the vsources SDK.
//!
//! Ports the TypeScript `src/error/index.js` hierarchy: every failure mode
//! a fetcher, provider, or extractor can produce has a typed variant so
//! callers can react (retry, skip, surface, or cool down) without string
//! matching.

use std::fmt;

use url::Url;

/// Why a request was blocked by anti-bot protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedReason {
    /// A Cloudflare challenge page was returned and no solver succeeded.
    CloudflareChallenge,
    /// A configured Cloudflare solver (e.g. `FlareSolverr`) failed.
    FlareSolverrFailed,
    /// Content was withheld by a Cloudflare censorship/geo rule.
    CloudflareCensor,
    /// An unknown blocking mechanism.
    Unknown,
}

impl fmt::Display for BlockedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::CloudflareChallenge => "cloudflare_challenge",
            Self::FlareSolverrFailed => "flaresolverr_failed",
            Self::CloudflareCensor => "cloudflare_censor",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}
/// Errors produced by the HTTP layer ([`crate::traits::Fetcher`]).
#[derive(Debug, Clone, thiserror::Error)]
pub enum FetchError {
    /// The request was blocked (see [`BlockedReason`]).
    #[error("blocked: {reason} for {url}")]
    Blocked {
        /// The URL that was blocked.
        url: Url,
        /// Why it was blocked.
        reason: BlockedReason,
    },
    /// The resource does not exist (HTTP 404 or equivalent).
    #[error("not found: {url}")]
    NotFound {
        /// The URL that was not found.
        url: Url,
    },
    /// The request timed out.
    #[error("timeout: {url}")]
    Timeout {
        /// The URL that timed out.
        url: Url,
    },
    /// The host rate-limited the request (HTTP 429).
    #[error("rate limited: {url} (retry after {retry_after_ms:?}ms)")]
    RateLimited {
        /// The URL that was rate limited.
        url: Url,
        /// Parsed `Retry-After` hint, when present.
        retry_after_ms: Option<u64>,
    },
    /// Too many timeouts for this host; it is temporarily evicted.
    #[error("too many timeouts: {url}")]
    TooManyTimeouts {
        /// The URL whose host has timed out too often.
        url: Url,
    },
    /// A non-specific HTTP error status.
    #[error("http {status} for {url}")]
    Http {
        /// The URL that failed.
        url: Url,
        /// The HTTP status code.
        status: u16,
    },
    /// The response body was not valid JSON.
    #[error("invalid JSON from {url}")]
    InvalidJson {
        /// The URL that produced invalid JSON.
        url: Url,
    },
    /// A transport-level failure (DNS, TLS, connection).
    #[error("transport error for {url}: {message}")]
    Transport {
        /// The URL that failed.
        url: Url,
        /// The underlying error message.
        message: String,
    },
}

impl FetchError {
    /// The URL this error relates to, when applicable.
    #[must_use]
    pub fn url(&self) -> Option<&Url> {
        match self {
            Self::Blocked { url, .. }
            | Self::NotFound { url }
            | Self::Timeout { url }
            | Self::RateLimited { url, .. }
            | Self::TooManyTimeouts { url }
            | Self::Http { url, .. }
            | Self::InvalidJson { url }
            | Self::Transport { url, .. } => Some(url),
        }
    }
}

/// Errors produced by providers ([`crate::traits::Source`]).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SourceError {
    /// The media reference could not be resolved on this provider.
    #[error("not found")]
    NotFound,
    /// The underlying HTTP request failed.
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    /// The upstream site returned an unexpected page or payload.
    #[error("scrape failed for {provider}: {message}")]
    Scrape {
        /// The provider id.
        provider: String,
        /// What went wrong.
        message: String,
    },
    /// TMDB metadata could not be resolved.
    #[error("tmdb error: {0}")]
    Tmdb(String),
    /// The domain resolver found no reachable base URL.
    #[error("no reachable domain for {domain_key}")]
    NoDomain {
        /// The provider's domain key.
        domain_key: String,
    },
}

impl SourceError {
    /// A scrape failure with a formatted message.
    pub fn scrape(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Scrape {
            provider: provider.into(),
            message: message.into(),
        }
    }
}

/// Errors produced by extractors ([`crate::traits::Extractor`]).
#[derive(Debug, Clone, thiserror::Error)]
pub enum ExtractorError {
    /// The embed could not be resolved to a direct stream.
    #[error("not found")]
    NotFound,
    /// The underlying HTTP request failed.
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    /// The embed page structure was unexpected.
    #[error("extraction failed for {extractor}: {message}")]
    Extraction {
        /// The extractor id.
        extractor: String,
        /// What went wrong.
        message: String,
    },
}

impl ExtractorError {
    /// An extraction failure with a formatted message.
    pub fn extraction(extractor: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Extraction {
            extractor: extractor.into(),
            message: message.into(),
        }
    }
}
