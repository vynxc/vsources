//! The three extension traits of the SDK: `Fetcher`, `Source`, `Extractor`.
//!
//! Every crate above `vsources-core` builds on these object-safe,
//! `Send + Sync` abstractions so the HTTP stack, the provider list, and
//! the extractor list can all be swapped or extended by embedders.

use std::collections::BTreeMap;

use async_trait::async_trait;
use url::Url;

use crate::error::{ExtractorError, FetchError, SourceError};
use crate::types::{MediaRef, SourceInfo, Stream};

/// An outgoing HTTP request.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// Target URL.
    pub url: Url,
    /// HTTP method (`GET`, `POST`, `HEAD`).
    pub method: String,
    /// Extra headers (Referer, X-Requested-With, …).
    pub headers: BTreeMap<String, String>,
    /// Request body for POST.
    pub body: Option<String>,
    /// Raw request body for binary APIs. Takes precedence over `body`.
    pub binary_body: Option<Vec<u8>>,
    /// Per-request timeout.
    pub timeout: Option<std::time::Duration>,
    /// Maximum redirects to follow (0 disables).
    pub max_redirects: Option<usize>,
    /// Per-host concurrency override.
    pub queue_limit: Option<usize>,
}

impl FetchRequest {
    /// A GET request.
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            binary_body: None,
            timeout: None,
            max_redirects: None,
            queue_limit: None,
        }
    }

    /// A POST request with a body.
    pub fn post(url: Url, body: impl Into<String>) -> Self {
        Self {
            method: "POST".to_string(),
            body: Some(body.into()),
            ..Self::get(url)
        }
    }

    /// A POST request carrying bytes without a text conversion.
    pub fn post_bytes(url: Url, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "POST".to_string(),
            binary_body: Some(body.into()),
            ..Self::get(url)
        }
    }

    /// A HEAD request.
    pub fn head(url: Url) -> Self {
        Self {
            method: "HEAD".to_string(),
            ..Self::get(url)
        }
    }

    /// Attach a header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
}

impl FetchRequest {
    /// Attach a per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// An incoming HTTP response.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// Final URL after redirects.
    pub url: Url,
    /// HTTP status code.
    pub status: u16,
    /// Response headers (lower-cased keys).
    pub headers: BTreeMap<String, String>,
    /// Body text (decoded lossily).
    pub body: String,
}

impl FetchResponse {
    /// A response header (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Whether the status is in the 2xx range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, FetchError> {
        serde_json::from_str(&self.body).map_err(|_| FetchError::InvalidJson {
            url: self.url.clone(),
        })
    }
}

/// The HTTP abstraction used by providers and extractors.
///
/// The default implementation lives in `vsources-net` (Chrome TLS
/// impersonation); embedders may supply their own (e.g. over `OkHttp` on
/// Android). The trait is dyn-compatible; the convenience helpers that
/// need generics ([`fetch_text`], [`fetch_json`], [`fetch_head`]) are free
/// functions taking `&dyn Fetcher`.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Execute a request.
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError>;

    /// Read at most `max_bytes` raw response bytes, then close the body.
    ///
    /// Used for media probes and bounded binary API responses. Implementations
    /// must enforce the limit even
    /// when a server ignores Range, and return HTTP error statuses intact.
    /// Probe failures must not populate host-wide block/timeout caches.
    /// The default returns `None` (inconclusive) without I/O, so existing
    /// custom fetchers remain compatible and never download an entire movie.
    async fn probe(
        &self,
        _request: FetchRequest,
        _max_bytes: usize,
    ) -> Result<Option<ProbeResponse>, FetchError> {
        Ok(None)
    }

    /// Follow redirects manually and return the final URL.
    async fn final_redirect_url(&self, url: Url) -> Result<Url, FetchError> {
        let response = self
            .request(FetchRequest::head(url.clone()).with_redirects_disabled())
            .await?;
        if (300..400).contains(&response.status)
            && let Some(location) = response.header("location")
            && let Ok(next) = url.join(location)
        {
            return Ok(next);
        }
        Ok(url)
    }
}

/// A bounded, binary response prefix for media validation.
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    /// Final URL after redirects; relative playlist paths use this base.
    pub url: Url,
    /// HTTP status, including error statuses.
    pub status: u16,
    /// Response headers with lower-cased names.
    pub headers: BTreeMap<String, String>,
    /// Raw prefix; never more than the requested limit.
    pub body: Vec<u8>,
    /// The prefix reached the limit, so the full body may be larger.
    pub truncated: bool,
}

impl ProbeResponse {
    /// A response header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// GET a URL as text.
pub async fn fetch_text(fetcher: &dyn Fetcher, url: Url) -> Result<String, FetchError> {
    let response = fetcher.request(FetchRequest::get(url)).await?;
    Ok(response.body)
}

/// GET a URL and parse the body as JSON.
pub async fn fetch_json<T: serde::de::DeserializeOwned>(
    fetcher: &dyn Fetcher,
    url: Url,
) -> Result<T, FetchError> {
    let response = fetcher
        .request(FetchRequest::get(url).with_header("Accept", "application/json"))
        .await?;
    response.json()
}

/// HEAD a URL and return its headers.
pub async fn fetch_head(
    fetcher: &dyn Fetcher,
    url: Url,
) -> Result<BTreeMap<String, String>, FetchError> {
    let response = fetcher.request(FetchRequest::head(url)).await?;
    Ok(response.headers)
}

impl FetchRequest {
    /// Disable redirect following for this request.
    #[must_use]
    pub fn with_redirects_disabled(mut self) -> Self {
        self.max_redirects = Some(0);
        self
    }
}

/// Shared context passed to providers and extractors during a resolve.
///
/// Carries the fetcher, TMDB client, and the clock so implementations stay
/// testable.
pub struct ResolveCtx<'a> {
    /// The HTTP layer.
    pub fetcher: &'a dyn Fetcher,
    /// Optional pre-resolved media metadata.
    pub media: Option<ResolvedMedia>,
    /// The calling provider's id, when known.
    ///
    /// Extractors fold it into cache keys and result labels so the same
    /// embed resolved from two sources can be distinguished.
    pub source_id: Option<&'a str>,
    /// The page a player embed was found on, when known.
    ///
    /// Hosts gate hotlinked media on `Referer`; extractors forward it both
    /// to their fetches and to the resulting stream's request headers.
    pub referer: Option<&'a Url>,
}

impl<'a> ResolveCtx<'a> {
    /// Attach the calling provider's id.
    #[must_use]
    pub fn with_source_id(mut self, source_id: &'a str) -> Self {
        self.source_id = Some(source_id);
        self
    }

    /// Attach the embed's referring page.
    #[must_use]
    pub fn with_referer(mut self, referer: &'a Url) -> Self {
        self.referer = Some(referer);
        self
    }
}

/// Media metadata resolved from TMDB (or supplied by the caller).
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    /// TMDB id, when resolved.
    pub tmdb_id: Option<u64>,
    /// `IMDb` id, when resolved.
    pub imdb_id: Option<String>,
    /// Title.
    pub name: String,
    /// Release year.
    pub year: Option<u16>,
    /// Season/episode context.
    pub season: Option<u32>,
    /// Episode number.
    pub episode: Option<u32>,
}

/// A streaming provider: resolves a [`MediaRef`] into streams.
#[async_trait]
pub trait Source: Send + Sync {
    /// Static description of this provider.
    fn info(&self) -> &SourceInfo;

    /// Resolve streams for a media reference.
    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError>;

    /// Resolve English spoken audio only.
    ///
    /// The default accepts explicit English-dub metadata; subtitle language
    /// flags and display labels alone are insufficient. Multi-audio providers
    /// can override this to verify and select an embedded English track.
    async fn resolve_english_dub(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let mut streams = self.resolve(ctx, media).await?;
        streams.retain(|stream| {
            !stream.is_external
                && stream.meta.dubbed == Some(true)
                && stream
                    .meta
                    .languages
                    .contains(&crate::types::CountryCode::En)
        });
        Ok(streams)
    }
}

/// An embed-URL extractor: resolves player URLs into direct streams.
#[async_trait]
pub trait Extractor: Send + Sync {
    /// The extractor's id.
    fn id(&self) -> &str;

    /// The extractor's display label.
    fn label(&self) -> &str;

    /// Whether this extractor handles the URL.
    fn supports(&self, ctx: &ResolveCtx<'_>, url: &Url) -> bool;

    /// Canonicalize a URL before extraction/caching.
    fn normalize(&self, url: &Url) -> Url {
        url.clone()
    }

    /// Resolve to canonical form asynchronously when the host must be
    /// fetched to know it (redirect chains, resolver APIs).
    ///
    /// The registry uses the result for cache keys; default is identity,
    /// mirroring the upstream base class.
    async fn normalize_async(&self, ctx: &ResolveCtx<'_>, url: &Url) -> Url {
        let _ = ctx;
        url.clone()
    }

    /// A cache-busting version for this extractor's results.
    ///
    /// Bump when extraction logic changes enough that old cached entries
    /// are wrong; the registry mixes it into the cache key.
    fn cache_version(&self) -> Option<u32> {
        None
    }

    /// Resolve the URL into direct streams.
    async fn extract(&self, ctx: &ResolveCtx<'_>, url: &Url)
    -> Result<Vec<Stream>, ExtractorError>;
}
