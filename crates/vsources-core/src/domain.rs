//! Base-URL discovery for providers with rotating domains.
//!
//! Ports `probeBaseUrl`/`isDomainAlive` and the dead-domain/eviction
//! bookkeeping from `src/source/Source.js`: environment overrides win over
//! a shared domains registry, which wins over racing the fallback
//! candidates; hosts that fail probes are marked dead for a day.
//!
//! Providers call [`DomainResolver::probe_base_url`] once per resolve with
//! their stable `domain_key` and a list of known mirrors.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::select_all;
use moka::sync::Cache as SyncCache;
use serde::Deserialize;
use tokio::sync::RwLock;
use url::Url;

use crate::error::{FetchError, SourceError};
use crate::traits::{FetchRequest, Fetcher, fetch_json};

/// Default shared domains registry (mirrors the upstream constant).
pub const DEFAULT_DOMAINS_URL: &str =
    "https://raw.githubusercontent.com/Anshu78780/json/main/providers.json";
/// TTL of the domains registry.
const DOMAINS_TTL: Duration = Duration::from_hours(4);
/// TTL of a successful base-URL probe.
const BASE_URL_TTL: Duration = Duration::from_hours(4);
/// How long a dead host stays dead.
const DEAD_DOMAIN_TTL: Duration = Duration::from_hours(24);
/// Probe timeout for liveness HEAD requests.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// Window in which a domain key must keep failing to be evicted.
const FAILURE_EVICTION_WINDOW: Duration = Duration::from_mins(5);

/// One registry entry: either a bare URL or an object with a `url` field.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RegistryEntry {
    /// `"key": "https://example.com"`
    Url(String),
    /// `"key": { "url": "https://example.com" }`
    Detailed {
        /// The URL the entry points at.
        url: Option<String>,
    },
}

impl RegistryEntry {
    /// The URL this entry points at, when present.
    fn url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url),
            Self::Detailed { url } => url.as_deref(),
        }
    }
}

/// Shared domain-resolution state; cheap to clone.
#[derive(Clone)]
pub struct DomainResolver {
    inner: Arc<DomainInner>,
}

struct DomainInner {
    fetcher: Arc<dyn Fetcher>,
    /// Registry URL, when one is configured.
    domains_url: Option<Url>,
    /// Registry with its fetch timestamp; stale value survives failures.
    registry: RwLock<RegistryState>,
    /// Successful base-URL probes by domain key.
    base_urls: SyncCache<String, Url>,
    /// Dead hosts, with the dead TTL.
    dead_domains: SyncCache<String, ()>,
    /// First failure timestamp per domain key.
    first_failure: std::sync::Mutex<HashMap<String, Instant>>,
}

/// The domains registry and when it was last fetched.
#[derive(Default)]
struct RegistryState {
    value: Option<Arc<HashMap<String, RegistryEntry>>>,
    fetched_at: Option<Instant>,
}

impl DomainResolver {
    /// Create a resolver using the default shared domains registry.
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self::build(fetcher, Url::parse(DEFAULT_DOMAINS_URL).ok())
    }

    /// Create a resolver reading from a custom registry URL.
    ///
    /// Returns `None` when the URL cannot be parsed.
    pub fn with_domains_url(fetcher: Arc<dyn Fetcher>, domains_url: &str) -> Option<Self> {
        Some(Self::build(fetcher, Url::parse(domains_url).ok()))
    }

    fn build(fetcher: Arc<dyn Fetcher>, domains_url: Option<Url>) -> Self {
        let inner = DomainInner {
            fetcher,
            domains_url,
            registry: RwLock::new(RegistryState::default()),
            base_urls: SyncCache::builder()
                .time_to_live(BASE_URL_TTL)
                .max_capacity(1024)
                .build(),
            dead_domains: SyncCache::builder()
                .time_to_live(DEAD_DOMAIN_TTL)
                .max_capacity(2048)
                .build(),
            first_failure: std::sync::Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Resolve the base URL for a provider's `domain_key`.
    ///
    /// Resolution order: `{KEY}_BASE_URL` environment override, a cached
    /// successful probe, the shared domains registry, then a race across
    /// `fallbacks`. Every fallback host is marked dead when none answers.
    pub async fn probe_base_url(
        &self,
        domain_key: &str,
        fallbacks: &[Url],
    ) -> Result<Url, SourceError> {
        if let Some(url) = env_override(domain_key) {
            return Ok(url);
        }
        if let Some(url) = self.inner.base_urls.get(domain_key) {
            return Ok(url);
        }

        if let Some(registry) = self.registry().await
            && let Some(candidate) = registry
                .get(domain_key)
                .and_then(RegistryEntry::url)
                .and_then(|url| Url::parse(url).ok())
            && !self.is_host_dead(&candidate)
            && self.is_domain_alive(&candidate).await
        {
            self.inner
                .base_urls
                .insert(domain_key.to_string(), candidate.clone());
            return Ok(candidate);
        }

        // Race the fallbacks, skipping dead hosts; when all are dead the
        // full list is retried anyway (matching the upstream behavior).
        let alive: Vec<Url> = fallbacks
            .iter()
            .filter(|url| !self.is_host_dead(url))
            .cloned()
            .collect();
        let candidates = if alive.is_empty() {
            fallbacks.to_vec()
        } else {
            alive
        };
        if let Ok(winner) = self.race_alive(candidates).await {
            self.inner
                .base_urls
                .insert(domain_key.to_string(), winner.clone());
            self.record_success(domain_key);
            return Ok(winner);
        }

        for fallback in fallbacks {
            self.mark_host_dead(fallback);
        }
        Err(SourceError::NoDomain {
            domain_key: domain_key.to_string(),
        })
    }

    /// Whether a HEAD probe of `url` gets any answer.
    ///
    /// Blocks, rate limits, and HTTP errors still prove the host is up;
    /// only timeouts and transport failures count as dead — exactly the
    /// upstream error classes.
    pub async fn is_domain_alive(&self, url: &Url) -> bool {
        let request = FetchRequest::head(url.clone()).with_timeout(PROBE_TIMEOUT);
        match self.inner.fetcher.request(request).await {
            Ok(_) => true,
            Err(error) => matches!(
                error,
                FetchError::Blocked { .. }
                    | FetchError::NotFound { .. }
                    | FetchError::RateLimited { .. }
                    | FetchError::TooManyTimeouts { .. }
                    | FetchError::Http { .. }
                    | FetchError::InvalidJson { .. }
            ),
        }
    }

    /// Whether `host` is currently marked dead.
    #[must_use]
    pub fn is_dead(&self, host: &str) -> bool {
        self.inner.dead_domains.contains_key(host)
    }

    /// Mark a host dead for the dead-domain TTL.
    pub fn mark_dead(&self, host: &str) {
        self.inner.dead_domains.insert(host.to_string(), ());
    }

    /// Record a resolve failure for a domain key.
    ///
    /// A second failure after the eviction window evicts the cached base
    /// URL and marks its host dead, forcing a fresh probe on the next
    /// resolve (the upstream `recordFailure` eviction).
    pub fn record_failure(&self, domain_key: &str) {
        let now = Instant::now();
        let evict = {
            let mut failures = lock(&self.inner.first_failure);
            match failures.get(domain_key) {
                None => {
                    failures.insert(domain_key.to_string(), now);
                    false
                }
                Some(first) if now.duration_since(*first) >= FAILURE_EVICTION_WINDOW => {
                    failures.remove(domain_key);
                    true
                }
                Some(_) => false,
            }
        };
        if evict && let Some(base) = self.inner.base_urls.get(domain_key) {
            if let Some(host) = base.host_str() {
                self.mark_dead(host);
            }
            self.inner.base_urls.invalidate(domain_key);
        }
    }

    /// Clear the failure state for a domain key after a success.
    pub fn record_success(&self, domain_key: &str) {
        lock(&self.inner.first_failure).remove(domain_key);
    }

    /// Whether a domain key has an unresolved first failure on record.
    #[must_use]
    pub fn is_failing(&self, domain_key: &str) -> bool {
        lock(&self.inner.first_failure).contains_key(domain_key)
    }

    /// The current registry, refreshed when stale.
    ///
    /// A failed refresh keeps serving the stale value; the next call
    /// retries immediately (the upstream never resets its timestamp on
    /// failure). The write lock doubles as single-flight, so concurrent
    /// probes share one registry fetch.
    async fn registry(&self) -> Option<Arc<HashMap<String, RegistryEntry>>> {
        let domains_url = self.inner.domains_url.clone()?;
        {
            let guard = self.inner.registry.read().await;
            if let Some(at) = guard.fetched_at
                && at.elapsed() < DOMAINS_TTL
            {
                return guard.value.clone();
            }
        }
        let mut guard = self.inner.registry.write().await;
        // Double-checked: another waiter may have refreshed while the
        // write lock was contended.
        if let Some(at) = guard.fetched_at
            && at.elapsed() < DOMAINS_TTL
        {
            return guard.value.clone();
        }
        // A failed fetch keeps the stale registry; the timestamp is not
        // advanced, so the next call retries the fetch.
        if let Ok(value) =
            fetch_json::<HashMap<String, RegistryEntry>>(self.inner.fetcher.as_ref(), domains_url)
                .await
        {
            guard.value = Some(Arc::new(value));
            guard.fetched_at = Some(Instant::now());
        }
        guard.value.clone()
    }

    /// Whether the URL's host is marked dead.
    fn is_host_dead(&self, url: &Url) -> bool {
        url.host_str().is_some_and(|host| self.is_dead(host))
    }

    /// Mark the URL's host dead.
    fn mark_host_dead(&self, url: &Url) {
        if let Some(host) = url.host_str() {
            self.mark_dead(host);
        }
    }

    /// Race the candidates; the first live one wins.
    ///
    /// Mirrors `Promise.any`: success short-circuits the remaining probes,
    /// and only universal failure is an error.
    async fn race_alive(&self, candidates: Vec<Url>) -> Result<Url, ()> {
        // Boxed because `select_all` requires `Unpin` futures.
        let probes: Vec<Pin<Box<_>>> = candidates
            .into_iter()
            .map(|url| Box::pin(probe_alive(self.clone(), url)))
            .collect();
        let mut probes = probes;
        loop {
            if probes.is_empty() {
                return Err(());
            }
            let (result, _index, rest) = select_all(probes).await;
            if let (url, true) = result {
                return Ok(url);
            }
            probes = rest;
        }
    }
}

/// Probe one candidate, returning it with its liveness.
async fn probe_alive(resolver: DomainResolver, url: Url) -> (Url, bool) {
    let alive = resolver.is_domain_alive(&url).await;
    (url, alive)
}

/// The `{KEY}_BASE_URL` override for a domain key, when set and valid.
fn env_override(domain_key: &str) -> Option<Url> {
    override_url(|name| std::env::var(name).ok(), domain_key)
}

/// Resolve an override URL for a domain key from a variable lookup.
///
/// Split from [`env_override`] so the mapping stays testable without
/// mutating process-global environment variables.
fn override_url(lookup: impl Fn(&str) -> Option<String>, domain_key: &str) -> Option<Url> {
    let value = lookup(&env_var_name(domain_key))?;
    Url::parse(&value).ok()
}

/// The override variable name for a domain key (`4khdhub_one` becomes
/// `4KHDHUB_ONE_BASE_URL`).
fn env_var_name(domain_key: &str) -> String {
    let sanitized: String = domain_key
        .to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{sanitized}_BASE_URL")
}

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A fetcher with canned behavior per host.
    struct MockFetcher {
        /// Hosts that answer probes (any HTTP status).
        alive: Vec<&'static str>,
        /// Hosts that time out.
        dead: Vec<&'static str>,
        /// Registry payload to serve, keyed by URL string.
        registry: Option<(&'static str, &'static str)>,
    }

    #[async_trait::async_trait]
    impl Fetcher for MockFetcher {
        async fn request(
            &self,
            request: FetchRequest,
        ) -> Result<crate::traits::FetchResponse, FetchError> {
            let host = request.url.host_str().unwrap_or_default().to_string();
            if let Some((url, body)) = self.registry
                && request.url.as_str() == url
            {
                return Ok(crate::traits::FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::new(),
                    body: body.to_string(),
                });
            }
            if self.alive.iter().any(|h| *h == host) {
                return Ok(crate::traits::FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::new(),
                    body: String::new(),
                });
            }
            if self.dead.iter().any(|h| *h == host) {
                return Err(FetchError::Timeout { url: request.url });
            }
            Err(FetchError::Transport {
                url: request.url,
                message: "unreachable".to_string(),
            })
        }
    }

    fn resolver(alive: Vec<&'static str>, dead: Vec<&'static str>) -> DomainResolver {
        let registry = Url::parse("https://registry.invalid/providers.json")
            .unwrap_or_else(|_| panic!("the registry URL is valid"));
        DomainResolver::build(
            Arc::new(MockFetcher {
                alive,
                dead,
                registry: None,
            }),
            Some(registry),
        )
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|_| panic!("{s} is a valid URL"))
    }

    #[test]
    fn override_lookup_wins() {
        // An injected lookup instead of a process-global env mutation.
        let override_value = override_url(
            |name| (name == "TESTKEY_BASE_URL").then(|| "https://override.example/".to_string()),
            "testkey",
        );
        assert_eq!(
            override_value.map(|u| u.as_str().to_string()),
            Some("https://override.example/".to_string())
        );
        // An unset variable and an invalid URL both resolve to no override.
        assert_eq!(override_url(|_| None, "testkey"), None);
        assert_eq!(
            override_url(|_| Some("not a url".to_string()), "testkey"),
            None
        );
    }

    #[tokio::test]
    async fn races_fallbacks_and_caches_the_winner() {
        let resolver = resolver(vec!["live.example"], vec!["dead.example"]);
        let base = resolver
            .probe_base_url(
                "somekey",
                &[url("https://dead.example/"), url("https://live.example/")],
            )
            .await
            .unwrap_or_else(|e| panic!("a live mirror must win: {e}"));
        assert_eq!(base.host_str(), Some("live.example"));
        // Cached: a second probe succeeds even with no live mirrors.
        let cached = resolver
            .probe_base_url("somekey", &[url("https://dead.example/")])
            .await
            .unwrap_or_else(|e| panic!("the winner must be cached: {e}"));
        assert_eq!(cached.host_str(), Some("live.example"));
    }

    #[tokio::test]
    async fn marks_all_fallbacks_dead_when_none_answers() {
        let resolver = resolver(vec![], vec!["down.example"]);
        let result = resolver
            .probe_base_url(
                "alldead",
                &[url("https://down.example/a"), url("https://down.example/b")],
            )
            .await;
        assert!(matches!(result, Err(SourceError::NoDomain { .. })));
        assert!(resolver.is_dead("down.example"));
    }

    #[tokio::test]
    async fn skips_dead_candidates_in_the_race() {
        // `down.example` is dead from the start, so the second fallback is
        // the only live candidate.
        let resolver = resolver(vec!["live2.example"], vec![]);
        resolver.mark_dead("down.example");
        let base = resolver
            .probe_base_url(
                "mixed",
                &[url("https://down.example/"), url("https://live2.example/")],
            )
            .await
            .unwrap_or_else(|e| panic!("the live mirror must win: {e}"));
        assert_eq!(base.host_str(), Some("live2.example"));
    }

    #[tokio::test]
    async fn uses_the_domains_registry() {
        let fetcher: Arc<MockFetcher> = Arc::new(MockFetcher {
            alive: vec!["registry-live.example"],
            dead: vec![],
            registry: Some((
                "https://registry.invalid/providers.json",
                r#"{"regkey":"https://registry-live.example/"}"#,
            )),
        });
        let resolver = DomainResolver::build(
            fetcher,
            Some(url("https://registry.invalid/providers.json")),
        );
        let base = resolver
            .probe_base_url("regkey", &[url("https://fallback.example/")])
            .await
            .unwrap_or_else(|e| panic!("registry must win: {e}"));
        assert_eq!(base.host_str(), Some("registry-live.example"));
    }

    #[tokio::test]
    async fn treats_blocks_and_http_errors_as_alive() {
        /// Fails with the given error kind for every request.
        struct FailingFetcher(FetchError);

        #[async_trait::async_trait]
        impl Fetcher for FailingFetcher {
            async fn request(
                &self,
                request: FetchRequest,
            ) -> Result<crate::traits::FetchResponse, FetchError> {
                Err(match self.0.clone() {
                    FetchError::Blocked { .. } => FetchError::Blocked {
                        url: request.url,
                        reason: crate::error::BlockedReason::CloudflareChallenge,
                    },
                    other => other,
                })
            }
        }

        let blocked: Arc<dyn Fetcher> = Arc::new(FailingFetcher(FetchError::Blocked {
            url: url("https://x.example/"),
            reason: crate::error::BlockedReason::CloudflareChallenge,
        }));
        let resolver = DomainResolver::build(
            blocked,
            Some(url("https://registry.invalid/providers.json")),
        );
        assert!(resolver.is_domain_alive(&url("https://x.example/")).await);

        let http: Arc<dyn Fetcher> = Arc::new(FailingFetcher(FetchError::Http {
            url: url("https://x.example/"),
            status: 500,
        }));
        let resolver =
            DomainResolver::build(http, Some(url("https://registry.invalid/providers.json")));
        assert!(resolver.is_domain_alive(&url("https://x.example/")).await);

        let timeout: Arc<dyn Fetcher> = Arc::new(FailingFetcher(FetchError::Timeout {
            url: url("https://x.example/"),
        }));
        let resolver = DomainResolver::build(
            timeout,
            Some(url("https://registry.invalid/providers.json")),
        );
        assert!(!resolver.is_domain_alive(&url("https://x.example/")).await);
    }

    #[test]
    fn evicts_after_a_window_of_failures() {
        let resolver = resolver(vec![], vec![]);
        resolver
            .inner
            .base_urls
            .insert("flaky".to_string(), url("https://flaky.example/"));
        // The first failure only records.
        resolver.record_failure("flaky");
        assert!(resolver.is_failing("flaky"));
        assert!(!resolver.is_dead("flaky.example"));
        // Backdate the first failure past the eviction window (the window
        // is far longer than any test run).
        lock(&resolver.inner.first_failure).insert(
            "flaky".to_string(),
            Instant::now()
                .checked_sub(FAILURE_EVICTION_WINDOW + Duration::from_secs(1))
                .unwrap_or_else(|| panic!("the eviction window is far shorter than uptime")),
        );
        // The next failure evicts and marks the host dead.
        resolver.record_failure("flaky");
        assert!(resolver.is_dead("flaky.example"));
        assert!(resolver.inner.base_urls.get("flaky").is_none());
        assert!(!resolver.is_failing("flaky"));
    }

    #[test]
    fn builds_env_variable_names() {
        assert_eq!(env_var_name("4khdhub_one"), "4KHDHUB_ONE_BASE_URL");
        assert_eq!(env_var_name("vega movies"), "VEGA_MOVIES_BASE_URL");
        // The real environment is untouched: no override exists for the key.
        assert_eq!(env_override("4khdhub_one"), None);
    }
}
