//! The default [`Fetcher`]: Chrome-impersonating HTTP with Cloudflare
//! bypass.
//!
//! Ports `src/utils/Fetcher.js`. The upstream stack used Node's plain
//! `https` first and fell back to `got-scraping` (Chrome TLS) on 403s;
//! `wreq` already *is* the Chrome-impersonating transport, so the layer
//! order inverts and simplifies:
//!
//! 1. `wreq` with a Chrome [`Profile`] — the fast, pooled default that
//!    passes TLS fingerprinting outright.
//! 2. A [`SolverChain`] (`FlareSolverr` now, `WebView` solvers later) — the
//!    escape hatch for hosts that still serve a managed challenge.
//! 3. Solved clearances are replayed as cookies on an immediate retry.
//! 4. No luck? The host lands in a negative cache for an hour
//!    (upstream `CF_DOMAIN_CACHE_TTL`) and fails fast until then.
//!
//! Per-host fairness from `queuedFetch` (semaphores), the
//! `TooManyTimeouts` eviction, and the 429 `Retry-After` mapping all
//! carry over.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use url::Url;
use wreq::Method;
use wreq::cookie::Jar;
use wreq::redirect::Policy;
use wreq_util::Profile;

use vsources_cloudflare::SolverChain;
use vsources_cloudflare::clearance::Clearance;
use vsources_cloudflare::detection::{self, Challenge};
use vsources_core::error::{BlockedReason, FetchError};
use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ProbeResponse};

use crate::blocked::BlockedHosts;
use crate::queue::HostSemaphores;
use crate::timeouts::TimeoutLedger;

/// Default per-request timeout (`DEFAULT_TIMEOUT`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default concurrent requests per host (`DEFAULT_QUEUE_LIMIT`).
pub const DEFAULT_QUEUE_LIMIT: usize = 50;
/// Default queue wait before giving up (`DEFAULT_QUEUE_TIMEOUT`).
pub const DEFAULT_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive timeouts before a host is evicted
/// (`DEFAULT_TIMEOUTS_COUNT_THROW`).
pub const DEFAULT_TIMEOUTS_COUNT_THROW: u32 = 30;
/// How long an eviction holds without a success.
pub const DEFAULT_TIMEOUT_EVICT_COOLOFF: Duration = Duration::from_secs(60);
/// How long a Cloudflare-blocked host fails fast (`CF_DOMAIN_CACHE_TTL`).
pub const CF_DOMAIN_CACHE_TTL: Duration = Duration::from_hours(1);
/// Redirect hops followed before stopping (`maxCount`).
pub const MAX_REDIRECTS: usize = 10;

/// The browser identity the upstream ships (`DEFAULT_USER_AGENT` is
/// Chrome 131).
pub const DEFAULT_PROFILE: Profile = Profile::Chrome131;

/// Default `Accept`, mirroring the upstream header set.
const DEFAULT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
/// Default `Accept-Language`, mirroring the upstream header set.
const DEFAULT_ACCEPT_LANGUAGE: &str = "en";

/// The default [`Fetcher`].
///
/// Built on `wreq` with Chrome TLS/HTTP2/header impersonation — the Rust
/// equivalent of `got-scraping` — plus the per-host queueing, timeout
/// eviction, and Cloudflare bookkeeping of the upstream `Fetcher` class.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use vsources_net::ChromeFetcher;
/// use vsources_core::traits::fetch_text;
///
/// let fetcher = ChromeFetcher::builder().build()?;
/// let page = fetch_text(&fetcher, url::Url::parse("https://example.com/")?).await?;
/// # Ok(())
/// # }
/// ```
pub struct ChromeFetcher {
    /// Redirect-following client.
    client: wreq::Client,
    /// Client with redirects off, for manual hops and `maxRedirects: 0`.
    no_redirect: wreq::Client,
    /// Cookie jar shared by both clients.
    jar: Arc<Jar>,
    /// Per-request timeout fallback.
    request_timeout: Duration,
    /// Per-host concurrency queue.
    queue: HostSemaphores,
    /// Consecutive-timeout breaker.
    timeouts: TimeoutLedger,
    /// Cloudflare negative cache.
    blocked: BlockedHosts,
    /// Optional solver chain for detected challenges.
    chain: Option<SolverChain>,
}

impl ChromeFetcher {
    /// A builder with the upstream defaults.
    #[must_use]
    pub fn builder() -> ChromeFetcherBuilder {
        ChromeFetcherBuilder::default()
    }

    /// The shared cookie jar — pre-seed it for hosts with known sessions.
    #[must_use]
    pub fn cookie_jar(&self) -> &Arc<Jar> {
        &self.jar
    }

    /// Set a cookie for a URL, ports `setCookie`.
    ///
    /// `cookie` uses `Set-Cookie` syntax (`"name=value; Path=/"`); a bare
    /// `"name=value"` works too and scopes to the URL's host.
    pub fn set_cookie(&self, url: &Url, cookie: impl AsRef<str>) {
        self.jar.add(cookie.as_ref(), url.as_str());
    }

    /// Store a solved clearance so future requests carry it.
    ///
    /// Each `name=value` pair from the clearance's [`Clearance`] cookie
    /// header is added to the jar for `url`'s host.
    fn store_clearance(&self, clearance: &Clearance, url: &Url) {
        for pair in clearance.cookie_header().split("; ") {
            self.jar.add(pair, url.as_str());
        }
    }

    /// Run one exchange over wreq and convert it into a
    /// [`FetchResponse`], updating the timeout ledger on the way.
    ///
    /// `user_agent` overrides the emulation's UA on the wire (used when
    /// replaying a clearance, which is bound to the solving browser).
    async fn execute(
        &self,
        request: &FetchRequest,
        user_agent: Option<&str>,
    ) -> Result<FetchResponse, FetchError> {
        let host = request.url.host_str().unwrap_or_default().to_string();
        let builder = self.request_builder(request, user_agent)?;

        let response = builder
            .send()
            .await
            .map_err(|error| self.map_transport(&request.url, &host, &error))?;
        self.timeouts.record_success(&host);

        let url = Url::parse(&response.uri().to_string()).unwrap_or_else(|_| request.url.clone());
        let status = response.status().as_u16();
        let headers = collect_headers(response.headers());
        let body = response
            .text()
            .await
            .map_err(|error| FetchError::Transport {
                url: request.url.clone(),
                message: error.to_string(),
            })?;
        Ok(FetchResponse {
            url,
            status,
            headers,
            body,
        })
    }

    /// Build the same browser request for scraping and bounded probes.
    fn request_builder(
        &self,
        request: &FetchRequest,
        user_agent: Option<&str>,
    ) -> Result<wreq::RequestBuilder, FetchError> {
        let client = if request.max_redirects == Some(0) {
            &self.no_redirect
        } else {
            &self.client
        };
        let method = Method::from_bytes(request.method.as_bytes()).map_err(|error| {
            FetchError::Transport {
                url: request.url.clone(),
                message: format!("invalid method {:?}: {error}", request.method),
            }
        })?;

        let mut builder = client
            .request(method, request.url.as_str())
            .timeout(request.timeout.unwrap_or(self.request_timeout));
        builder = apply_default_headers(builder, &request.headers);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(user_agent) = user_agent {
            builder = builder.header("user-agent", user_agent);
        }
        if let Some(body) = &request.binary_body {
            builder = builder.body(body.clone());
        } else if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        Ok(builder)
    }

    /// Translate a wreq transport error, feeding the timeout ledger.
    fn map_transport(&self, url: &Url, host: &str, error: &wreq::Error) -> FetchError {
        if error.is_timeout() {
            if self.timeouts.record_timeout(host) {
                tracing::warn!(host, "too many consecutive timeouts; evicting the host");
            }
            FetchError::Timeout { url: url.clone() }
        } else {
            FetchError::Transport {
                url: url.clone(),
                message: error.to_string(),
            }
        }
    }

    /// Map an unchallenged response onto the error taxonomy.
    fn map_status(response: FetchResponse) -> Result<FetchResponse, FetchError> {
        match response.status {
            404 => Err(FetchError::NotFound {
                url: response.url.clone(),
            }),
            429 => Err(FetchError::RateLimited {
                url: response.url.clone(),
                retry_after_ms: response
                    .header("retry-after")
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(|seconds| seconds * 1000),
            }),
            status if status >= 400 => Err(FetchError::Http {
                url: response.url.clone(),
                status,
            }),
            _ => Ok(response),
        }
    }

    /// Handle a detected Cloudflare challenge.
    ///
    /// Solvable challenges go to the [`SolverChain`] (when configured);
    /// a successful solve is replayed as cookies plus the solver's user
    /// agent on one retry. Everything else — no chain, a failed solve, a
    /// still-challenged retry, or an unsolvable block — marks the host in
    /// the negative cache and returns [`FetchError::Blocked`].
    async fn handle_challenge(
        &self,
        request: &FetchRequest,
        challenge: Challenge,
    ) -> Result<FetchResponse, FetchError> {
        let host = request.url.host_str().unwrap_or_default();
        if challenge.is_solvable()
            && let Some(chain) = &self.chain
        {
            match chain.solve(&request.url, challenge).await {
                Ok(clearance) => {
                    self.store_clearance(&clearance, &request.url);
                    match self.execute(request, Some(&clearance.user_agent)).await {
                        Ok(retry) => {
                            if detection::detect(&retry).is_none() {
                                return Self::map_status(retry);
                            }
                            // The clearance stopped working mid-flight;
                            // drop it so the next solve is fresh.
                            tracing::warn!(host, "a fresh clearance was still challenged");
                            chain.invalidate(&request.url);
                        }
                        // The retry itself timed out or failed at the
                        // transport layer: that is not a Cloudflare
                        // verdict, so surface it unchanged.
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => {
                    tracing::warn!(host, "cloudflare solve failed: {error}");
                    self.blocked.mark(host, BlockedReason::FlareSolverrFailed);
                    return Err(FetchError::Blocked {
                        url: request.url.clone(),
                        reason: BlockedReason::FlareSolverrFailed,
                    });
                }
            }
        }

        let reason = challenge.blocked_reason();
        self.blocked.mark(host, reason);
        Err(FetchError::Blocked {
            url: request.url.clone(),
            reason,
        })
    }
}

#[async_trait]
impl Fetcher for ChromeFetcher {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let host = request.url.host_str().unwrap_or_default().to_string();

        // Fast-fail gates, in the upstream order: a cached Cloudflare
        // block first, then a timed-out host.
        if let Some(reason) = self.blocked.check(&host) {
            return Err(FetchError::Blocked {
                url: request.url,
                reason,
            });
        }
        if self.timeouts.is_evicted(&host) {
            return Err(FetchError::TooManyTimeouts { url: request.url });
        }

        let _permit = self
            .queue
            .acquire(&host, request.queue_limit, &request.url)
            .await?;

        let response = self.execute(&request, None).await?;
        if let Some(challenge) = detection::detect(&response) {
            return self.handle_challenge(&request, challenge).await;
        }
        Self::map_status(response)
    }

    async fn probe(
        &self,
        request: FetchRequest,
        max_bytes: usize,
    ) -> Result<Option<ProbeResponse>, FetchError> {
        let host = request.url.host_str().unwrap_or_default();
        let _permit = self
            .queue
            .acquire(host, request.queue_limit, &request.url)
            .await?;
        // A probe is advisory. Do not consult or poison the scraper's
        // hour-long Cloudflare cache or trigger expensive solver attempts.
        let transport = |error: wreq::Error| FetchError::Transport {
            url: request.url.clone(),
            message: error.to_string(),
        };
        let response = self
            .request_builder(&request, None)?
            .send()
            .await
            .map_err(transport)?;
        let url = Url::parse(&response.uri().to_string()).unwrap_or_else(|_| request.url.clone());
        let status = response.status().as_u16();
        let headers = collect_headers(response.headers());
        let mut chunks = response.bytes_stream();
        let mut body = Vec::new();
        while body.len() < max_bytes {
            let Some(chunk) = chunks.next().await else {
                break;
            };
            let chunk = chunk.map_err(transport)?;
            let take = chunk.len().min(max_bytes - body.len());
            body.extend_from_slice(&chunk[..take]);
        }
        Ok(Some(ProbeResponse {
            url,
            status,
            headers,
            truncated: body.len() == max_bytes,
            body,
        }))
    }

    async fn final_redirect_url(&self, url: Url) -> Result<Url, FetchError> {
        // Ports getFinalRedirectUrl: walk Location headers manually, one
        // HEAD per hop, up to MAX_REDIRECTS; then give up and return where
        // we ended up.
        let mut current = url;
        for _ in 0..MAX_REDIRECTS {
            let response = self
                .request(FetchRequest::head(current.clone()).with_redirects_disabled())
                .await?;
            if (300..400).contains(&response.status)
                && let Some(location) = response.header("location")
                && let Ok(next) = current.join(location)
            {
                current = next;
                continue;
            }
            return Ok(current);
        }
        Ok(current)
    }
}

/// Add the upstream default headers unless the caller set their own.
fn apply_default_headers(
    builder: wreq::RequestBuilder,
    overrides: &BTreeMap<String, String>,
) -> wreq::RequestBuilder {
    let has = |name: &str| overrides.keys().any(|key| key.eq_ignore_ascii_case(name));
    let mut builder = builder;
    if !has("accept") {
        builder = builder.header("accept", DEFAULT_ACCEPT);
    }
    if !has("accept-language") {
        builder = builder.header("accept-language", DEFAULT_ACCEPT_LANGUAGE);
    }
    builder
}

/// Flatten a header map into lower-cased keys, joining repeated headers.
fn collect_headers(headers: &wreq::header::HeaderMap) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, value) in headers {
        let entry = map
            .entry(name.as_str().to_string())
            .or_insert_with(String::new);
        if !entry.is_empty() {
            entry.push_str(", ");
        }
        entry.push_str(&String::from_utf8_lossy(value.as_bytes()));
    }
    map
}

/// Builder for [`ChromeFetcher`].
pub struct ChromeFetcherBuilder {
    profile: Profile,
    request_timeout: Duration,
    queue_limit: usize,
    queue_timeout: Duration,
    timeout_threshold: u32,
    timeout_cooloff: Duration,
    cf_block_ttl: Duration,
    chain: Option<SolverChain>,
    proxy: Option<String>,
    ipv4_only: bool,
}

impl Default for ChromeFetcherBuilder {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE,
            request_timeout: DEFAULT_TIMEOUT,
            queue_limit: DEFAULT_QUEUE_LIMIT,
            queue_timeout: DEFAULT_QUEUE_TIMEOUT,
            timeout_threshold: DEFAULT_TIMEOUTS_COUNT_THROW,
            timeout_cooloff: DEFAULT_TIMEOUT_EVICT_COOLOFF,
            cf_block_ttl: CF_DOMAIN_CACHE_TTL,
            chain: None,
            proxy: None,
            ipv4_only: false,
        }
    }
}

impl ChromeFetcherBuilder {
    /// Emulate a different browser profile.
    ///
    /// The default matches the upstream's Chrome 131 user agent. Switch
    /// profiles to match the browser your solver actually solved with:
    /// `cf_clearance` is bound to the solving browser's identity.
    #[must_use]
    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Per-request timeout fallback (default [`DEFAULT_TIMEOUT`]).
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Concurrent requests per host (default [`DEFAULT_QUEUE_LIMIT`]).
    #[must_use]
    pub fn queue_limit(mut self, limit: usize) -> Self {
        self.queue_limit = limit;
        self
    }

    /// How long a request may wait for a host slot
    /// (default [`DEFAULT_QUEUE_TIMEOUT`]).
    #[must_use]
    pub fn queue_timeout(mut self, wait: Duration) -> Self {
        self.queue_timeout = wait;
        self
    }

    /// Consecutive timeouts before a host is evicted
    /// (default [`DEFAULT_TIMEOUTS_COUNT_THROW`]).
    #[must_use]
    pub fn timeout_threshold(mut self, timeouts: u32) -> Self {
        self.timeout_threshold = timeouts;
        self
    }

    /// How long an eviction holds without a success
    /// (default [`DEFAULT_TIMEOUT_EVICT_COOLOFF`]).
    #[must_use]
    pub fn timeout_cooloff(mut self, cooloff: Duration) -> Self {
        self.timeout_cooloff = cooloff;
        self
    }

    /// How long a Cloudflare-blocked host fails fast
    /// (default [`CF_DOMAIN_CACHE_TTL`]).
    #[must_use]
    pub fn cf_block_ttl(mut self, ttl: Duration) -> Self {
        self.cf_block_ttl = ttl;
        self
    }

    /// Solve detected Cloudflare challenges with `chain` and retry once
    /// with the earned clearance.
    #[must_use]
    pub fn cloudflare(mut self, chain: SolverChain) -> Self {
        self.chain = Some(chain);
        self
    }

    /// Route traffic through a proxy (`http://…` or `socks5://…`).
    #[must_use]
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.proxy = Some(proxy.into());
        self
    }

    /// Bind connections to IPv4, ports the upstream `family: 4`.
    ///
    /// The upstream forced IPv4 to dodge `ENETUNREACH` on hosts whose
    /// IPv6 route is broken; this is environment-specific, so it is opt-in
    /// here.
    #[must_use]
    pub fn ipv4_only(mut self, ipv4_only: bool) -> Self {
        self.ipv4_only = ipv4_only;
        self
    }

    /// Build the fetcher.
    ///
    /// Errors when the TLS/HTTP2 emulation stack cannot be initialized
    /// (bad proxy URL, system TLS state).
    pub fn build(self) -> Result<ChromeFetcher, wreq::Error> {
        let jar = Arc::new(Jar::default());
        let client = Self::build_client(
            self.profile,
            &jar,
            Policy::limited(MAX_REDIRECTS),
            self.ipv4_only,
            self.proxy.as_deref(),
        )?;
        let no_redirect = Self::build_client(
            self.profile,
            &jar,
            Policy::none(),
            self.ipv4_only,
            self.proxy.as_deref(),
        )?;
        Ok(ChromeFetcher {
            client,
            no_redirect,
            jar,
            request_timeout: self.request_timeout,
            queue: HostSemaphores::new(self.queue_limit, self.queue_timeout),
            timeouts: TimeoutLedger::new(self.timeout_threshold, self.timeout_cooloff),
            blocked: BlockedHosts::new(self.cf_block_ttl),
            chain: self.chain,
        })
    }

    /// One wreq client with the shared jar and a given redirect policy.
    fn build_client(
        profile: Profile,
        jar: &Arc<Jar>,
        redirects: Policy,
        ipv4_only: bool,
        proxy: Option<&str>,
    ) -> Result<wreq::Client, wreq::Error> {
        let mut builder = wreq::Client::builder()
            .emulation(profile)
            .cookie_provider(jar.clone())
            .redirect(redirects);
        if ipv4_only {
            builder = builder.local_address(IpAddr::from(Ipv4Addr::UNSPECIFIED));
        }
        if let Some(proxy) = proxy {
            let proxy = wreq::Proxy::all(proxy)?;
            builder = builder.proxy(proxy);
        }
        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use vsources_cloudflare::solver::{CloudflareSolver, SolveError};
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, Fetcher};

    use super::*;

    fn page_url(server: &MockServer, path: &str) -> Url {
        Url::parse(&format!("{}/{path}", server.uri())).unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A solver that hands out a fixed clearance.
    struct StubSolver {
        user_agent: &'static str,
        solved: std::sync::atomic::AtomicUsize,
    }

    impl StubSolver {
        fn new(user_agent: &'static str) -> Self {
            Self {
                user_agent,
                solved: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn solve_count(&self) -> usize {
            self.solved.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CloudflareSolver for StubSolver {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn can_solve(&self, challenge: Challenge) -> bool {
            challenge.is_solvable()
        }

        async fn solve(&self, url: &Url, _challenge: Challenge) -> Result<Clearance, SolveError> {
            self.solved
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Clearance::new(
                "token",
                None,
                self.user_agent,
                url.host_str().unwrap_or(""),
            ))
        }
    }

    #[tokio::test]
    async fn fetches_text_and_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .and(header("accept-language", "en"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));
        let response = fetcher
            .request(FetchRequest::get(page_url(&server, "page")))
            .await;

        let response = response.unwrap_or_else(|e| panic!("the fetch must succeed: {e}"));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
        assert_eq!(response.header("content-type"), Some("text/plain"));
    }

    #[tokio::test]
    async fn maps_status_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "4"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/broken"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));

        let gone = fetcher
            .request(FetchRequest::get(page_url(&server, "gone")))
            .await;
        match gone {
            Err(FetchError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        let limited = fetcher
            .request(FetchRequest::get(page_url(&server, "limited")))
            .await;
        match limited {
            Err(FetchError::RateLimited { retry_after_ms, .. }) => {
                assert_eq!(retry_after_ms, Some(4000), "retry-after must map to ms");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        let broken = fetcher
            .request(FetchRequest::get(page_url(&server, "broken")))
            .await;
        match broken {
            Err(FetchError::Http { status, .. }) => assert_eq!(status, 500),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn challenge_without_solver_blocks_and_fails_fast() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/guarded"))
            .respond_with(ResponseTemplate::new(403).insert_header("cf-mitigated", "challenge"))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));
        let url = page_url(&server, "guarded");

        let first = fetcher.request(FetchRequest::get(url.clone())).await;
        match first {
            Err(FetchError::Blocked { reason, .. }) => {
                assert_eq!(reason, BlockedReason::CloudflareChallenge);
            }
            other => panic!("expected Blocked, got {other:?}"),
        }

        // The second request must fail from the negative cache without
        // touching the network.
        let second = fetcher.request(FetchRequest::get(url)).await;
        match second {
            Err(FetchError::Blocked { reason, .. }) => {
                assert_eq!(reason, BlockedReason::CloudflareChallenge);
            }
            other => panic!("expected a fast-fail Blocked, got {other:?}"),
        }
        let hits = server.received_requests().await;
        assert_eq!(
            hits.as_ref().map_or(0, Vec::len),
            1,
            "the cached block must not re-fetch"
        );
    }

    #[tokio::test]
    async fn solves_and_retries_with_clearance() {
        let server = MockServer::start().await;
        // The guarded route: challenged for the emulation UA, fine once
        // the solver's UA and clearance cookie arrive.
        Mock::given(method("GET"))
            .and(path("/guarded"))
            .and(header("user-agent", "solve-agent"))
            .and(header("cookie", "cf_clearance=token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("cleared"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/guarded"))
            .respond_with(ResponseTemplate::new(403).insert_header("cf-mitigated", "challenge"))
            .mount(&server)
            .await;

        let solver = Arc::new(StubSolver::new("solve-agent"));
        let chain = SolverChain::new(vec![solver.clone()]);
        let fetcher = ChromeFetcher::builder()
            .cloudflare(chain)
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));

        let response = fetcher
            .request(FetchRequest::get(page_url(&server, "guarded")))
            .await
            .unwrap_or_else(|e| panic!("the retried fetch must succeed: {e}"));
        assert_eq!(response.body, "cleared");
        assert_eq!(solver.solve_count(), 1, "the challenge must be solved once");

        // Later requests carry the stored clearance directly: no new
        // solve, no challenge round-trip.
        let cached = fetcher
            .request(FetchRequest::get(page_url(&server, "guarded")))
            .await
            .unwrap_or_else(|e| panic!("the cached clearance must work: {e}"));
        assert_eq!(cached.body, "cleared");
        assert_eq!(
            solver.solve_count(),
            1,
            "the cookie jar must serve the clearance"
        );
    }

    #[tokio::test]
    async fn cookies_persist_across_requests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("set-cookie", "session=42; Path=/"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/echo"))
            .and(header("cookie", "session=42"))
            .respond_with(ResponseTemplate::new(200).set_body_string("seen"))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));
        fetcher
            .request(FetchRequest::get(page_url(&server, "start")))
            .await
            .unwrap_or_else(|e| panic!("the first request must succeed: {e}"));
        let response = fetcher
            .request(FetchRequest::get(page_url(&server, "echo")))
            .await
            .unwrap_or_else(|e| panic!("the second request must succeed: {e}"));
        assert_eq!(response.body, "seen", "the jar must replay the cookie");
    }

    #[tokio::test]
    async fn final_redirect_url_walks_every_hop() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/b"))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/b"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/c"))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/c"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));
        let final_url = fetcher
            .final_redirect_url(page_url(&server, "a"))
            .await
            .unwrap_or_else(|e| panic!("the redirect walk must succeed: {e}"));
        assert_eq!(final_url.path(), "/c", "the walk must reach the last hop");
    }

    #[tokio::test]
    async fn request_timeouts_are_reported_and_evict() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let fetcher = ChromeFetcher::builder()
            .request_timeout(Duration::from_millis(100))
            .timeout_threshold(2)
            .timeout_cooloff(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|error| panic!("the fetcher must build: {error}"));
        let url = page_url(&server, "slow");

        for attempt in 1..=2 {
            match fetcher.request(FetchRequest::get(url.clone())).await {
                Err(FetchError::Timeout { .. }) => {}
                other => panic!("attempt {attempt} must time out, got {other:?}"),
            }
        }
        match fetcher.request(FetchRequest::get(url)).await {
            Err(FetchError::TooManyTimeouts { .. }) => {}
            other => panic!("expected eviction, got {other:?}"),
        }
        let hits = server.received_requests().await;
        assert_eq!(
            hits.as_ref().map_or(0, Vec::len),
            2,
            "an evicted host must not be fetched again"
        );
    }
    #[tokio::test]
    async fn probe_stops_at_limit_even_when_server_ignores_range()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let (done, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            let (mut socket, _) = listener.accept()?;
            socket.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut request = [0; 4096];
            assert!(socket.read(&mut request)? > 0);
            // Advertise a huge file but deliberately hold the rest open.
            socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000000\r\nConnection: close\r\n\r\n",
            )?;
            socket.write_all(&[0x47; 4096])?;
            let _ = wait.recv_timeout(Duration::from_secs(5));
            Ok(())
        });
        let fetcher = ChromeFetcher::builder().build()?;
        let request = FetchRequest::get(Url::parse(&format!("http://{address}/movie"))?)
            .with_header("Range", "bytes=0-2047");
        let result =
            tokio::time::timeout(Duration::from_secs(2), fetcher.probe(request, 2048)).await;
        let _ = done.send(());
        server.join().unwrap_or_else(|_| panic!("server thread"))?;
        let response = result??.ok_or("probe unsupported")?;
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), 2048);
        assert!(response.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn probe_returns_http_errors_without_poisoning_scraper_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(path("/challenge"))
            .respond_with(ResponseTemplate::new(403).insert_header("cf-mitigated", "challenge"))
            .mount(&server)
            .await;
        Mock::given(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let fetcher = ChromeFetcher::builder().build()?;
        let response = fetcher
            .probe(FetchRequest::get(page_url(&server, "challenge")), 2048)
            .await?
            .ok_or("probe unsupported")?;
        assert_eq!(response.status, 403);
        assert_eq!(
            fetcher
                .request(FetchRequest::get(page_url(&server, "page")))
                .await?
                .body,
            "ok"
        );
        Ok(())
    }
    #[tokio::test]
    async fn binary_post_and_response_preserve_non_utf8_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let payload = vec![0, 255, 128, 1, 0];
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_bytes(payload.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![255, 0, 128, 254]))
            .mount(&server)
            .await;
        let fetcher = ChromeFetcher::builder().build()?;
        let response = fetcher
            .probe(
                FetchRequest::post_bytes(page_url(&server, "binary"), payload),
                1024,
            )
            .await?
            .ok_or("unsupported")?;
        assert_eq!(response.body, vec![255, 0, 128, 254]);
        assert!(!response.truncated);
        Ok(())
    }
}
