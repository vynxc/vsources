//! The `arm.haglund.dev` mapping API client.
//!
//! ARM serves Fribb's anime-mapping dataset (AODB ∪ Anime-Lists merged on
//! `AniDB`, TMDB gap-filled), refreshed every 24 h — the same bridge the
//! production Stremio anime addons resolve through. The `/api/v2/imdb` and
//! `/api/v2/themoviedb` routes exist precisely because an `IMDb`/`TMDB` id is
//! one-id-per-show: both answer the full per-season entry array, e.g. for
//! `Reincarnated as a Sword` (imdb `tt15483602`, tmdb `134667`):
//!
//! ```json
//! [
//!   {"anilist":139587,"myanimelist":49891,"imdb":"tt15483602",
//!    "themoviedb":134667,"themoviedb-season":1,...},
//!   {"anilist":159042,"myanimelist":53913,"imdb":"tt15483602",
//!    "themoviedb":134667,"themoviedb-season":2,...}
//! ]
//! ```
//!
//! Season rows with `themoviedb-season: 0` (OVAs/specials, seen for AOT) are
//! preserved in the returned list — callers filter on the season they want,
//! and providers keep their existing OVA behavior.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::error::SourceError;
use crate::traits::{FetchRequest, Fetcher};

/// The ARM API root.
const ARM_BASE_URL: &str = "https://arm.haglund.dev/api/v2";
/// TTL for the per-show entry arrays (ARM refreshes daily).
const ENTRY_TTL: Duration = Duration::from_hours(24);
/// Capacity of the entry cache.
const MAX_ENTRIES: u64 = 2048;
/// Default backoff before the single retry after a rate limit.
const RETRY_DELAY: Duration = Duration::from_millis(600);
/// Total attempts per request.
const ATTEMPTS: usize = 2;
/// One request timeout — failures are transient and never cached as misses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One per-season ARM entry — only the fields the mapping chain reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeasonEntry {
    /// The `AniList` id.
    #[serde(default, rename = "anilist")]
    pub anilist_id: Option<u64>,
    /// The `MyAnimeList` id.
    #[serde(default, rename = "myanimelist")]
    pub mal_id: Option<u64>,
    /// The `AniDB` id.
    #[serde(default, rename = "anidb")]
    pub anidb_id: Option<u64>,
    /// The `IMDb` id (present on every entry of an `IMDb`-keyed show).
    #[serde(default)]
    pub imdb: Option<String>,
    /// The `TMDB` id.
    #[serde(default, rename = "themoviedb")]
    pub tmdb_id: Option<u64>,
    /// The TMDB season this entry covers (0 = OVA/special rows).
    #[serde(default, rename = "themoviedb-season")]
    pub tmdb_season: Option<u32>,
}

impl SeasonEntry {
    /// The entry covering TMDB `season`, if this is it.
    ///
    /// A missing season marker is treated as season 1 (the plain-show row).
    #[must_use]
    pub fn matches_season(&self, season: u32) -> bool {
        self.tmdb_season.unwrap_or(1) == season
    }
}

/// A shared ARM client with a TTL cache.
///
/// Cheap to clone; all clones share one cache and one HTTP layer. Construct
/// one per application and hand it to the [`MappingService`](crate::mappings::service::MappingService).
#[derive(Clone)]
pub struct ArmClient {
    inner: Arc<ArmInner>,
}

struct ArmInner {
    fetcher: Arc<dyn Fetcher>,
    /// API root (a string so a bad override fails as an error, not a panic).
    base_url: String,
    /// Entry arrays by lookup key (`imdb:{id}` / `tmdb:{id}`).
    entries: moka::future::Cache<String, Vec<SeasonEntry>>,
}

impl ArmClient {
    /// Create a client over the shared fetcher.
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self::build(fetcher, ARM_BASE_URL)
    }

    /// Point the client at a custom API root (self-hosted mirrors).
    #[must_use]
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self::build(self.inner.fetcher.clone(), &base_url.into())
    }

    fn build(fetcher: Arc<dyn Fetcher>, base_url: &str) -> Self {
        Self {
            inner: Arc::new(ArmInner {
                fetcher,
                base_url: base_url.trim_end_matches('/').to_string(),
                entries: moka::future::Cache::builder()
                    .time_to_live(ENTRY_TTL)
                    .max_capacity(MAX_ENTRIES)
                    .build(),
            }),
        }
    }

    /// The per-season entries for an `IMDb` id, or `None` when ARM has no
    /// mapping for the show (a real miss — transport errors answer `Err`).
    pub async fn seasons_by_imdb(
        &self,
        imdb: &str,
    ) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        self.fetch_seasons("imdb", imdb).await
    }

    /// The per-season entries for a `TMDB` id.
    pub async fn seasons_by_tmdb(
        &self,
        tmdb: u64,
    ) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        self.fetch_seasons("themoviedb", &tmdb.to_string()).await
    }

    /// One route fetch through the TTL cache.
    async fn fetch_seasons(
        &self,
        route: &str,
        id: &str,
    ) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        let key = format!("{route}:{id}");
        if let Some(entries) = self.inner.entries.get(&key).await {
            return Ok(Some(entries));
        }

        let (route, id) = key.split_once(':').unwrap_or(("", key.as_str()));
        let url = Url::parse(&format!("{}/{route}?id={id}", self.inner.base_url))
            .map_err(|_| SourceError::scrape("arm", format!("invalid ARM URL for {key:?}")))?;
        let entries = self.fetch_json(&url).await?;
        self.inner.entries.insert(key, entries.clone()).await;
        Ok(Some(entries))
    }

    /// Fetch and decode one ARM route, with 429-aware retry.
    async fn fetch_json(&self, url: &Url) -> Result<Vec<SeasonEntry>, SourceError> {
        let request = FetchRequest::get(url.clone())
            .with_header("Accept", "application/json")
            .with_timeout(REQUEST_TIMEOUT);
        for attempt in 0..ATTEMPTS {
            let response = self.inner.fetcher.request(request.clone()).await?;
            if response.status == 404 {
                return Ok(Vec::new());
            }
            if response.is_success() {
                // Malformed successful responses are errors, never cached as misses.
                return response
                    .json::<Vec<SeasonEntry>>()
                    .map_err(SourceError::from);
            }
            if response.status != 429 || attempt + 1 == ATTEMPTS {
                break;
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
        Err(SourceError::scrape(
            "arm",
            format!("ARM request failed for {url}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;

    use super::*;
    use crate::error::FetchError;
    use crate::traits::FetchResponse;

    /// A fetcher serving canned bodies by URL path.
    struct MockArm {
        status: u16,
        body: String,
    }

    #[async_trait]
    impl Fetcher for MockArm {
        async fn request(&self, _request: FetchRequest) -> Result<FetchResponse, FetchError> {
            if self.status == 404 {
                return Err(FetchError::Http {
                    url: Url::parse("https://arm.haglund.dev/api/v2/imdb?id=tt0")
                        .unwrap_or_else(|e| panic!("valid test URL: {e}")),
                    status: 404,
                });
            }
            Ok(FetchResponse {
                url: Url::parse("https://arm.haglund.dev/api/v2/imdb?id=tt1")
                    .unwrap_or_else(|e| panic!("valid test URL: {e}")),
                status: self.status,
                headers: BTreeMap::new(),
                body: self.body.clone(),
            })
        }
    }

    /// The captured research fixture: both season rows for `tt15483602`.
    const SWORD: &str = r#"[
        {"anidb":16785,"anilist":139587,"animecountdown":1736387,"animenewsnetwork":24757,"anime-planet":"reincarnated-as-a-sword","anisearch":16618,"imdb":"tt15483602","kitsu":45242,"livechart":10805,"myanimelist":49891,"media":"TV","simkl":1736387,"themoviedb":134667,"themoviedb-season":1,"thetvdb":410378,"thetvdb-season":1},
        {"anidb":17789,"anilist":159042,"animecountdown":2086926,"animenewsnetwork":26756,"anime-planet":"reincarnated-as-a-sword-2nd-season","anisearch":17958,"imdb":"tt15483602","kitsu":46917,"livechart":11748,"myanimelist":53913,"media":"TV","simkl":2086926,"themoviedb":134667,"themoviedb-season":2,"thetvdb":410378,"thetvdb-season":2}
    ]"#;

    fn client(body: &str) -> ArmClient {
        ArmClient::new(Arc::new(MockArm {
            status: 200,
            body: body.to_string(),
        }))
    }

    #[tokio::test]
    async fn parses_per_season_entries() {
        let entries = client(SWORD)
            .seasons_by_imdb("tt15483602")
            .await
            .unwrap_or_else(|e| panic!("arm fetch must succeed: {e}"))
            .unwrap_or_else(|| panic!("arm must answer entries"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].anilist_id, Some(139_587));
        assert_eq!(entries[0].mal_id, Some(49_891));
        assert_eq!(entries[0].tmdb_season, Some(1));
        assert_eq!(entries[1].anilist_id, Some(159_042));
        assert_eq!(entries[1].mal_id, Some(53_913));
        assert_eq!(entries[1].tmdb_season, Some(2));
    }

    #[test]
    fn season_filtering_distinguishes_rows() {
        let entries: Vec<SeasonEntry> = serde_json::from_str(SWORD)
            .unwrap_or_else(|e| panic!("the sword fixture must parse: {e}"));
        assert!(entries[0].matches_season(1));
        assert!(!entries[0].matches_season(2));
        assert!(entries[1].matches_season(2));
        // A row with no season marker counts as the plain-show S1 row.
        let plain: SeasonEntry =
            serde_json::from_str(r#"{"anilist":1}"#).unwrap_or_else(|e| panic!("fixture: {e}"));
        assert!(plain.matches_season(1));
        assert!(!plain.matches_season(2));
        // Season 0 (OVA) rows never match a normal season request.
        let ova: SeasonEntry = serde_json::from_str(r#"{"anilist":2,"themoviedb-season":0}"#)
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        assert!(!ova.matches_season(1));
    }

    #[tokio::test]
    async fn empty_arm_response_answers_no_entries() {
        let entries = client("[]")
            .seasons_by_imdb("tt0000000")
            .await
            .unwrap_or_else(|e| panic!("empty arm must not error: {e}"));
        assert_eq!(entries, Some(Vec::new()));
    }

    #[tokio::test]
    async fn transport_failure_is_an_error_not_a_miss() {
        let arm = ArmClient::new(Arc::new(MockArm {
            status: 503,
            body: String::new(),
        }));
        let result = arm.seasons_by_imdb("tt15483602").await;
        assert!(
            result.is_err(),
            "a 503 must surface as an error, never a cached miss"
        );
    }
}
