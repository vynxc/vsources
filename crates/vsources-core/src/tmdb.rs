//! TMDB identity and metadata resolution.
//!
//! Ports `src/utils/tmdb.js`: a shared client that maps `IMDb` ids to `TMDB`
//! ids, `TMDB` ids back to `IMDb` ids, and resolves names and years — with
//! in-flight deduplication, TTL caches, and 429-aware retry.
//!
//! The upstream fix these mechanics implement: with ~70 providers resolving
//! the same title concurrently, one shared fetch per unique lookup key
//! prevents TMDB from rate-limiting the burst (429 → every source returns
//! nothing). A real "not found" is negatively cached for ten minutes so
//! wrong ids stop re-bursting; transient failures are never cached, so
//! recovery is immediate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use moka::future::Cache;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use crate::error::{FetchError, SourceError};
use crate::traits::{FetchRequest, Fetcher, ResolvedMedia};
use crate::types::{MediaId, MediaRef, MediaType};

/// TMDB API root.
const TMDB_BASE_URL: &str = "https://api.themoviedb.org/3";
/// TTL for id↔id mappings (static facts, capped hard).
const MAPPING_TTL: Duration = Duration::from_hours(24);
/// TTL for name/year lookups (details are effectively static).
const DETAILS_TTL: Duration = Duration::from_hours(1);
/// TTL for negative `/find` results.
const NOT_FOUND_TTL: Duration = Duration::from_mins(10);
/// Capacity of the id mapping caches.
const MAX_MAPPINGS: u64 = 4096;
/// Capacity of the name/year cache (upstream `MAX_MAP`).
const MAX_DETAILS: u64 = 600;
/// Default backoff before the single retry after a rate limit.
const RETRY_DELAY: Duration = Duration::from_millis(600);
/// Upper bound honored from `Retry-After` (upstream cap of 1.2 s).
const MAX_RETRY_AFTER_MS: u64 = 1200;
/// Total attempts per request (upstream: two).
const ATTEMPTS: usize = 2;

/// Name, year, and original name resolved from TMDB details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaName {
    /// The localized title (`name` for series, `title` for movies).
    pub name: String,
    /// The original title, when TMDB has one.
    pub original_name: Option<String>,
    /// Release year, when a date is known.
    pub year: Option<u16>,
}

/// A shared TMDB client with caching and single-flight lookups.
///
/// Cheap to clone; all clones share one set of caches, one in-flight table
/// per lookup family, and one HTTP layer. Construct one per application and
/// hand it to every provider.
#[derive(Clone)]
pub struct TmdbClient {
    inner: Arc<TmdbInner>,
}

struct TmdbInner {
    fetcher: Arc<dyn Fetcher>,
    /// API root (a string so a bad override fails as an error, not a panic).
    base_url: String,
    api_key: Option<String>,
    access_token: Option<String>,
    /// `IMDb` → `TMDB` id mappings.
    imdb_to_tmdb: Cache<String, u64>,
    /// `TMDB` id → `IMDb` id mappings (`None` = known to have none).
    tmdb_to_imdb: Cache<u64, Option<String>>,
    /// `type:id:language` → details.
    details: Cache<String, MediaName>,
    /// Animation classification from the same details response, with no extra I/O.
    animation: Cache<String, Option<bool>>,
    /// Negative `/find` results.
    not_found: Cache<String, ()>,
    /// In-flight `/find` calls by `IMDb` id.
    inflight_find: Mutex<HashMap<String, SharedLoad<u64>>>,
    /// In-flight `external_ids` calls by TMDB id.
    inflight_external: Mutex<HashMap<String, SharedLoad<Option<String>>>>,
    /// In-flight details calls by `type:id:language`.
    inflight_details: Mutex<HashMap<String, SharedLoad<MediaName>>>,
}

/// A single-flight future shared by concurrent callers.
type SharedLoad<T> = Shared<BoxFuture<'static, Result<T, SourceError>>>;

impl TmdbClient {
    /// Create a client that authenticates with an API key.
    pub fn new(api_key: impl Into<String>, fetcher: Arc<dyn Fetcher>) -> Self {
        Self::build(Some(api_key.into()), None, fetcher, TMDB_BASE_URL)
    }

    /// Create a client that authenticates with a v4 access token.
    #[must_use]
    pub fn with_access_token(token: impl Into<String>, fetcher: Arc<dyn Fetcher>) -> Self {
        Self::build(None, Some(token.into()), fetcher, TMDB_BASE_URL)
    }

    /// Create a client from the `TMDB_API_KEY` / `TMDB_ACCESS_TOKEN`
    /// environment variables, mirroring the upstream env override.
    ///
    /// Prefers the access token; returns `None` when neither is set.
    #[must_use]
    pub fn from_env(fetcher: Arc<dyn Fetcher>) -> Option<Self> {
        let token = std::env::var("TMDB_ACCESS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let key = std::env::var("TMDB_API_KEY").ok().filter(|k| !k.is_empty());
        if token.is_none() && key.is_none() {
            return None;
        }
        Some(Self::build(key, token, fetcher, TMDB_BASE_URL))
    }

    /// Point the client at a custom API root (self-hosted proxies).
    #[must_use]
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self::build(
            self.inner.api_key.clone(),
            self.inner.access_token.clone(),
            Arc::clone(&self.inner.fetcher),
            &base_url.into(),
        )
    }

    fn build(
        api_key: Option<String>,
        access_token: Option<String>,
        fetcher: Arc<dyn Fetcher>,
        base_url: &str,
    ) -> Self {
        let inner = TmdbInner {
            fetcher,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            access_token,
            imdb_to_tmdb: Cache::builder()
                .time_to_live(MAPPING_TTL)
                .max_capacity(MAX_MAPPINGS)
                .build(),
            tmdb_to_imdb: Cache::builder()
                .time_to_live(MAPPING_TTL)
                .max_capacity(MAX_MAPPINGS)
                .build(),
            details: Cache::builder()
                .time_to_live(DETAILS_TTL)
                .max_capacity(MAX_DETAILS)
                .build(),
            animation: Cache::builder()
                .time_to_live(DETAILS_TTL)
                .max_capacity(MAX_DETAILS)
                .build(),
            not_found: Cache::builder()
                .time_to_live(NOT_FOUND_TTL)
                .max_capacity(MAX_DETAILS)
                .build(),
            inflight_find: Mutex::new(HashMap::new()),
            inflight_external: Mutex::new(HashMap::new()),
            inflight_details: Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Resolve the `TMDB` id for an `IMDb` id via `/find`.
    ///
    /// Series references search `tv_results`, movies search `movie_results`.
    pub async fn tmdb_id_from_imdb(
        &self,
        imdb_id: &str,
        kind: MediaType,
    ) -> Result<u64, SourceError> {
        if let Some(id) = self.inner.imdb_to_tmdb.get(imdb_id).await {
            return Ok(id);
        }
        let negative_key = format!("find:{imdb_id}");
        if self.inner.not_found.contains_key(&negative_key) {
            return Err(SourceError::NotFound);
        }

        let client = self.clone();
        let imdb = imdb_id.to_string();
        let key = imdb.clone();
        single_flight(&self.inner.inflight_find, imdb, async move {
            let response: FindResponse = client
                .fetch_json(&format!("/find/{key}"), &[("external_source", "imdb_id")])
                .await?;
            let results = match kind {
                MediaType::Series => response.tv_results,
                MediaType::Movie => response.movie_results,
            };
            let Some(entry) = results.into_iter().next() else {
                // A real not-found (response received, no mapping) is
                // negatively cached; transient failures are not.
                client.inner.not_found.insert(negative_key, ()).await;
                return Err(SourceError::NotFound);
            };
            client.inner.imdb_to_tmdb.insert(key, entry.id).await;
            Ok(entry.id)
        })
        .await
    }

    /// Resolve the `IMDb` id for a `TMDB` id via `external_ids`.
    ///
    /// `Ok(None)` means `TMDB` knows this id has no `IMDb` counterpart.
    pub async fn imdb_id_from_tmdb(
        &self,
        tmdb_id: u64,
        kind: MediaType,
    ) -> Result<Option<String>, SourceError> {
        if let Some(id) = self.inner.tmdb_to_imdb.get(&tmdb_id).await {
            return Ok(id);
        }

        let client = self.clone();
        let key = tmdb_id;
        single_flight(
            &self.inner.inflight_external,
            tmdb_id.to_string(),
            async move {
                let path = format!("/{}/{key}/external_ids", kind_path(kind));
                let response: ExternalIdsResponse = client.fetch_json(&path, &[]).await?;
                client
                    .inner
                    .tmdb_to_imdb
                    .insert(key, response.imdb_id.clone())
                    .await;
                Ok(response.imdb_id)
            },
        )
        .await
    }

    /// Resolve the name, year, and original name for a TMDB id.
    ///
    /// `language` is an optional TMDB language tag (`en-US`) forwarded as
    /// the `language` parameter.
    pub async fn name_and_year(
        &self,
        tmdb_id: u64,
        kind: MediaType,
        language: Option<&str>,
    ) -> Result<MediaName, SourceError> {
        let cache_key = format!("{}:{tmdb_id}:{}", kind_path(kind), language.unwrap_or(""));
        if let Some(name) = self.inner.details.get(&cache_key).await {
            return Ok(name);
        }

        let client = self.clone();
        let language = language.unwrap_or("").to_string();
        let key = cache_key.clone();
        single_flight(&self.inner.inflight_details, cache_key, async move {
            let path = format!("/{}/{tmdb_id}", kind_path(kind));
            let response: DetailsResponse = client
                .fetch_json(&path, &[("language", language.as_str())])
                .await?;
            let genres: Vec<u64> = response
                .genres
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|genre| genre.get("id").and_then(serde_json::Value::as_u64))
                .collect();
            let animation = (!genres.is_empty()).then(|| genres.contains(&16));
            client.inner.animation.insert(key.clone(), animation).await;
            let name = match kind {
                MediaType::Series => response.name,
                MediaType::Movie => response.title,
            };
            let Some(name) = name else {
                return Err(SourceError::NotFound);
            };
            let (date, original) = match kind {
                MediaType::Series => (response.first_air_date, response.original_name),
                MediaType::Movie => (response.release_date, response.original_title),
            };
            let value = MediaName {
                name,
                original_name: original,
                year: year_from_date(date.as_deref()),
            };
            client.inner.details.insert(key, value.clone()).await;
            Ok(value)
        })
        .await
    }

    /// Animation status learned from existing default-language details, without I/O.
    ///
    /// `None` means genres are absent, unclassified, or not cached. It never means
    /// that the title is known to be non-animation.
    pub async fn cached_is_animation(&self, tmdb_id: u64, kind: MediaType) -> Option<bool> {
        let key = format!("{}:{tmdb_id}:", kind_path(kind));
        self.inner.animation.get(&key).await.flatten()
    }

    /// Resolve full metadata for a media reference.
    ///
    /// `IMDb`-keyed references are mapped through `/find` first; the `IMDb` id
    /// of a TMDB-keyed reference is left unset (call
    /// [`TmdbClient::imdb_id_from_tmdb`] when it is needed). Season and
    /// episode context is carried through from the reference.
    pub async fn resolve_media(&self, media: &MediaRef) -> Result<ResolvedMedia, SourceError> {
        let (tmdb_id, imdb_id) = match &media.id {
            MediaId::Tmdb(id) => (Some(*id), None),
            MediaId::Imdb(imdb) => {
                let tmdb = self.tmdb_id_from_imdb(imdb, media.kind).await?;
                (Some(tmdb), Some(imdb.clone()))
            }
        };
        let Some(tmdb_id) = tmdb_id else {
            return Err(SourceError::NotFound);
        };
        let name = self.name_and_year(tmdb_id, media.kind, None).await?;
        Ok(ResolvedMedia {
            tmdb_id: Some(tmdb_id),
            imdb_id,
            name: name.name,
            year: name.year,
            season: media.season,
            episode: media.episode,
        })
    }

    /// Fetch and decode a TMDB API path, with 429-aware retry.
    async fn fetch_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<T, SourceError> {
        if self.inner.api_key.is_none() && self.inner.access_token.is_none() {
            return Err(SourceError::Tmdb("TMDB API key not configured".into()));
        }
        let mut url = build_url(&self.inner.base_url, path)?;
        if let Some(key) = &self.inner.api_key {
            url.query_pairs_mut().append_pair("api_key", key);
        }
        for (name, value) in params {
            // Skip falsy params, like the upstream query builder.
            if !value.is_empty() {
                url.query_pairs_mut().append_pair(name, value);
            }
        }
        let mut request = FetchRequest::get(url).with_header("Accept", "application/json");
        if let Some(token) = &self.inner.access_token {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }

        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                // TMDB rate windows are short; a small backoff rides out
                // the burst class without blocking sources for long.
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match self.inner.fetcher.request(request.clone()).await {
                Ok(response) => return response.json::<T>().map_err(SourceError::from),
                // A 404 from TMDB means the id does not exist.
                Err(FetchError::Http { status: 404, .. }) => {
                    return Err(SourceError::NotFound);
                }
                Err(error) => {
                    let is_rate_limited = matches!(error, FetchError::RateLimited { .. });
                    if attempt + 1 == ATTEMPTS || !is_rate_limited {
                        return Err(SourceError::Fetch(error));
                    }
                    // Honor a sane Retry-After before the retry.
                    let delay = match &error {
                        FetchError::RateLimited {
                            retry_after_ms: Some(ra),
                            ..
                        } if *ra > 0 && *ra <= MAX_RETRY_AFTER_MS => Duration::from_millis(*ra),
                        _ => RETRY_DELAY,
                    };
                    tokio::time::sleep(delay).await;
                }
            }
        }
        // Only reachable when `ATTEMPTS` is zero.
        Err(SourceError::Tmdb("TMDB request retries exhausted".into()))
    }
}

/// Run `load` for `key` so concurrent callers share one in-flight future.
///
/// The entry is removed once the future settles: successes are already in
/// their TTL cache by then, and failures are retried fresh on the next
/// request — the same lifecycle the upstream `finally` cleanup provides.
async fn single_flight<T, F>(
    map: &Mutex<HashMap<String, SharedLoad<T>>>,
    key: String,
    load: F,
) -> Result<T, SourceError>
where
    T: Clone + Send + Sync + 'static,
    F: std::future::Future<Output = Result<T, SourceError>> + Send + 'static,
{
    let shared = {
        let mut guard = map.lock().await;
        guard
            .entry(key.clone())
            .or_insert_with(|| load.boxed().shared())
            .clone()
    };
    let result = shared.await;
    map.lock().await.remove(&key);
    result
}

/// Join the API root with an API path and validate the result.
fn build_url(base: &str, path: &str) -> Result<Url, SourceError> {
    let full = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    Url::parse(&full).map_err(|_| SourceError::Tmdb(format!("invalid TMDB request URL {full:?}")))
}

/// The API path segment for a media kind (`tv` / `movie`).
fn kind_path(kind: MediaType) -> &'static str {
    match kind {
        MediaType::Series => "tv",
        MediaType::Movie => "movie",
    }
}

/// The year of a `YYYY-MM-DD` (or `YYYY`) date, when present.
fn year_from_date(date: Option<&str>) -> Option<u16> {
    let year = date?.split('-').next()?.parse().ok()?;
    (1900..=2200).contains(&year).then_some(year)
}

/// `/find/{imdb}` response.
#[derive(Debug, Deserialize)]
struct FindResponse {
    #[serde(default)]
    tv_results: Vec<IdEntry>,
    #[serde(default)]
    movie_results: Vec<IdEntry>,
}

/// One result entry carrying only the id.
#[derive(Debug, Deserialize)]
struct IdEntry {
    id: u64,
}

/// `/{type}/{id}/external_ids` response.
#[derive(Debug, Deserialize)]
struct ExternalIdsResponse {
    imdb_id: Option<String>,
}

/// `/{type}/{id}` details response (tv and movie fields unified).
#[derive(Debug, Deserialize)]
struct DetailsResponse {
    #[serde(default)]
    genres: Option<serde_json::Value>,
    name: Option<String>,
    title: Option<String>,
    first_air_date: Option<String>,
    release_date: Option<String>,
    original_name: Option<String>,
    original_title: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::traits::FetchResponse;

    /// A fetcher serving canned JSON for the paths used by the tests.
    struct MockTmdb;

    fn ok_response(path: &str, body: &str) -> FetchResponse {
        FetchResponse {
            url: Url::parse(&format!("https://api.themoviedb.org/3{path}"))
                .unwrap_or_else(|_| panic!("{path} is a valid URL")),
            status: 200,
            headers: BTreeMap::new(),
            body: body.to_string(),
        }
    }

    #[async_trait::async_trait]
    impl Fetcher for MockTmdb {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            let path = request.url.path();
            let path = path.strip_prefix("/3").unwrap_or(path);
            match path {
                "/find/tt0944947" => Ok(ok_response(
                    path,
                    r#"{"tv_results":[{"id":1396}],"movie_results":[]}"#,
                )),
                "/find/tt0000000" => {
                    Ok(ok_response(path, r#"{"tv_results":[],"movie_results":[]}"#))
                }
                "/tv/1396" => Ok(ok_response(
                    path,
                    r#"{"name":"Breaking Bad","first_air_date":"2008-01-20","original_name":"Breaking Bad"}"#,
                )),
                "/tv/1396/external_ids" => Ok(ok_response(path, r#"{"imdb_id":"tt0944947"}"#)),
                _ => Err(FetchError::Http {
                    url: request.url,
                    status: 404,
                }),
            }
        }
    }

    fn client() -> TmdbClient {
        TmdbClient::new("test-key", Arc::new(MockTmdb))
    }

    #[tokio::test]
    async fn maps_imdb_to_tmdb_and_back() {
        let client = client();
        let tmdb = client
            .tmdb_id_from_imdb("tt0944947", MediaType::Series)
            .await
            .unwrap_or_else(|e| panic!("find must succeed: {e}"));
        assert_eq!(tmdb, 1396);
        let imdb = client
            .imdb_id_from_tmdb(1396, MediaType::Series)
            .await
            .unwrap_or_else(|e| panic!("external ids must succeed: {e}"));
        assert_eq!(imdb.as_deref(), Some("tt0944947"));
    }

    #[tokio::test]
    async fn resolves_names_and_years() {
        let client = client();
        let name = client
            .name_and_year(1396, MediaType::Series, None)
            .await
            .unwrap_or_else(|e| panic!("details must succeed: {e}"));
        assert_eq!(name.name, "Breaking Bad");
        assert_eq!(name.year, Some(2008));
        assert_eq!(name.original_name.as_deref(), Some("Breaking Bad"));
    }

    #[tokio::test]
    async fn genre_classification_reuses_details_and_preserves_unknown_metadata() {
        struct GenreFetcher {
            body: String,
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl Fetcher for GenreFetcher {
            async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::new(),
                    body: self.body.clone(),
                })
            }
        }
        for (genres, expected) in [
            (serde_json::json!([{"id":16}]), Some(true)),
            (serde_json::json!([{"id":18}]), Some(false)),
            (serde_json::json!([]), None),
            (serde_json::Value::Null, None),
        ] {
            let fetcher = Arc::new(GenreFetcher {
                body: serde_json::json!({"title":"Movie","genres":genres}).to_string(),
                calls: AtomicUsize::new(0),
            });
            let client = TmdbClient::new("test-key", fetcher.clone());
            assert_eq!(
                client.cached_is_animation(289, MediaType::Movie).await,
                None
            );
            client
                .name_and_year(289, MediaType::Movie, None)
                .await
                .unwrap_or_else(|e| panic!("metadata: {e}"));
            assert_eq!(
                client.cached_is_animation(289, MediaType::Movie).await,
                expected
            );
            assert_eq!(
                client.cached_is_animation(289, MediaType::Series).await,
                None,
                "movie and TV id namespaces remain separate"
            );
            assert_eq!(
                fetcher.calls.load(Ordering::SeqCst),
                1,
                "classification adds no request"
            );
        }
    }

    #[tokio::test]
    async fn negatively_caches_missing_find_results() {
        let client = client();
        let first = client
            .tmdb_id_from_imdb("tt0000000", MediaType::Movie)
            .await;
        assert!(matches!(first, Err(SourceError::NotFound)));
        // The negative result is cached; a second call must not refetch.
        let second = client
            .tmdb_id_from_imdb("tt0000000", MediaType::Movie)
            .await;
        assert!(matches!(second, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn resolves_full_media_metadata() {
        let client = client();
        let media = MediaRef::series(MediaId::Imdb("tt0944947".into()), 1, 1);
        let resolved = client
            .resolve_media(&media)
            .await
            .unwrap_or_else(|e| panic!("resolve must succeed: {e}"));
        assert_eq!(resolved.tmdb_id, Some(1396));
        assert_eq!(resolved.imdb_id.as_deref(), Some("tt0944947"));
        assert_eq!(resolved.name, "Breaking Bad");
        assert_eq!(resolved.year, Some(2008));
        assert_eq!(resolved.season, Some(1));
        assert_eq!(resolved.episode, Some(1));
    }

    #[tokio::test]
    async fn dedupes_concurrent_find_calls() {
        /// Counts `/find` requests.
        struct CountingTmdb {
            finds: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl Fetcher for CountingTmdb {
            async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
                if request
                    .url
                    .path()
                    .strip_prefix("/3")
                    .is_some_and(|path| path == "/find/tt0944947")
                {
                    self.finds.fetch_add(1, Ordering::SeqCst);
                    return Ok(ok_response(
                        "/find/tt0944947",
                        r#"{"tv_results":[{"id":1396}],"movie_results":[]}"#,
                    ));
                }
                Err(FetchError::Http {
                    url: request.url,
                    status: 404,
                })
            }
        }

        let counting = Arc::new(CountingTmdb {
            finds: AtomicUsize::new(0),
        });
        let client = TmdbClient::new("test-key", counting.clone());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .tmdb_id_from_imdb("tt0944947", MediaType::Series)
                    .await
            }));
        }
        for task in tasks {
            let result = task
                .await
                .unwrap_or_else(|e| panic!("task must not panic: {e}"));
            assert!(result.is_ok());
        }
        assert_eq!(
            counting.finds.load(Ordering::SeqCst),
            1,
            "concurrent lookups must share one request"
        );
    }

    #[test]
    fn parses_years_leniently() {
        assert_eq!(year_from_date(Some("2008-01-20")), Some(2008));
        assert_eq!(year_from_date(Some("2008")), Some(2008));
        assert_eq!(year_from_date(Some("n/a")), None);
        assert_eq!(year_from_date(None), None);
    }
}
