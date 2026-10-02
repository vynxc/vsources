//! The shared provider-mapping façade.
//!
//! [`MappingService`] folds the [`ArmClient`]
//! and [`AniListClient`] bridges
//! behind the id-first resolution the anime providers use:
//!
//! 1. `arm` — the per-season entry array for the show's `IMDb`/`TMDB` id,
//!    filtered to the requested season → the target `AniList`/MAL id;
//! 2. `anilist` title search — the fallback when ARM has no mapping (its
//!    coverage stops at anime with an `IMDb`/`TMDB` entry);
//! 3. nothing — providers keep their title-scoring as the last tier.
//!
//! The critical property is the **single flight**: with ~20 anime providers
//! resolving the same show concurrently, one shared fetch per lookup key
//! issues one ARM call instead of twenty — the same medicine
//! [`TmdbClient`](crate::tmdb::TmdbClient) prescribes for the TMDB burst.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::Mutex;

use crate::error::SourceError;
use crate::mappings::anilist::{AniListClient, AniListMedia};
use crate::mappings::arm::{ArmClient, SeasonEntry};
use crate::traits::Fetcher;

/// The resolved ids for one (show, season) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonIds {
    /// The `AniList` id of the season's catalog entry.
    pub anilist_id: u64,
    /// The `MyAnimeList` id, when the source carries one.
    pub mal_id: Option<u64>,
}

impl SeasonIds {
    /// The MAL id, when present; the `AniList` id otherwise.
    #[must_use]
    pub fn mal_or_anilist(&self) -> u64 {
        self.mal_id.unwrap_or(self.anilist_id)
    }
}

/// The shared mapping service: cheap-clone, shared caches, single-flight.
#[derive(Clone)]
pub struct MappingService {
    arm: ArmClient,
    anilist: AniListClient,
    /// In-flight `seasons` calls by lookup key.
    inflight_seasons: Arc<Mutex<HashMap<String, SharedSeason>>>,
    /// In-flight anilist searches by query.
    inflight_search: Arc<Mutex<HashMap<String, SharedSearch>>>,
    inflight_id: Arc<Mutex<HashMap<String, SharedLoad<Option<AniListMedia>>>>>,
}

/// A single-flight future shared by concurrent callers.
type SharedSeason = Shared<BoxFuture<'static, Result<Option<Vec<SeasonEntry>>, SourceError>>>;
/// A single-flight search future.
type SharedSearch = Shared<BoxFuture<'static, Result<Vec<AniListMedia>, SourceError>>>;

impl MappingService {
    /// Create the service over the shared fetcher.
    ///
    /// One per application, handed to every provider (the arm/anilist caches
    /// and the in-flight tables are shared through the cheap clone).
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            arm: ArmClient::new(Arc::clone(&fetcher)),
            anilist: AniListClient::new(fetcher),
            inflight_seasons: Arc::new(Mutex::new(HashMap::new())),
            inflight_search: Arc::new(Mutex::new(HashMap::new())),
            inflight_id: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The raw arm client (for bridges that speak the wire shape directly).
    #[must_use]
    pub fn arm(&self) -> &ArmClient {
        &self.arm
    }

    /// The raw anilist client (for the `anichan`-style GraphQL speakers).
    #[must_use]
    pub fn anilist(&self) -> &AniListClient {
        &self.anilist
    }

    /// Resolve the anime ids for `(imdb, season)`, `None` when no source
    /// knows the mapping.
    ///
    /// ARM first; the independent `AniList` search handles a missing mapping
    /// or an ARM outage. If both fail, callers retain their title fallback.
    pub async fn season_ids_by_imdb(
        &self,
        imdb: &str,
        season: u32,
        title: Option<&str>,
    ) -> Result<Option<SeasonIds>, SourceError> {
        let key = format!("imdb:{imdb}");
        let entries = self.seasons(&key, Some(imdb), None, title).await?;
        Ok(ids_for_season(entries.as_deref(), season))
    }

    /// Resolve the anime ids for `(tmdb, season)`.
    pub async fn season_ids_by_tmdb(
        &self,
        tmdb: u64,
        season: u32,
        title: Option<&str>,
    ) -> Result<Option<SeasonIds>, SourceError> {
        let key = format!("tmdb:{tmdb}");
        let entries = self.seasons(&key, None, Some(tmdb), title).await?;
        Ok(ids_for_season(entries.as_deref(), season))
    }

    /// One canonical `AniList` identity lookup, shared across title-only adapters.
    pub async fn anilist_by_id(&self, id: u64) -> Result<Option<AniListMedia>, SourceError> {
        let client = self.anilist.clone();
        single_flight(&self.inflight_id, id.to_string(), async move {
            client.by_id(id).await
        })
        .await
    }

    /// One anilist title search, deduped across concurrent callers.
    pub async fn anilist_search(&self, query: &str) -> Result<Vec<AniListMedia>, SourceError> {
        let key = query.to_string();
        let client = self.anilist.clone();
        let search = query.to_string();
        single_flight(&self.inflight_search, key, async move {
            client.search(&search).await
        })
        .await
    }

    /// The per-show entry array — one shared fetch per key.
    ///
    /// A real ARM miss or outage falls through to `AniList` when a title is supplied.
    async fn seasons(
        &self,
        key: &str,
        imdb: Option<&str>,
        tmdb: Option<u64>,
        title: Option<&str>,
    ) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        let entries = match self.seasons_single_flight(key, imdb, tmdb).await {
            Ok(entries) => entries,
            Err(error) => {
                // An independent-host fallback also applies to ARM outages.
                if let Some(title) = title
                    && let Ok(Some(entries)) = self.anilist_fallback(title).await
                {
                    return Ok(Some(entries));
                }
                return Err(error);
            }
        };
        if entries.as_ref().is_none_or(Vec::is_empty) && title.is_some() {
            // The anilist fallback: search the title and lift the season's
            // entry into the SeasonEntry shape.
            let title = title.unwrap_or_default();
            if let Some(entries) = self.anilist_fallback(title).await? {
                return Ok(Some(entries));
            }
        }
        Ok(entries)
    }

    /// One shared arm fetch per lookup key.
    async fn seasons_single_flight(
        &self,
        key: &str,
        imdb: Option<&str>,
        tmdb: Option<u64>,
    ) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        let arm = self.arm.clone();
        let imdb = imdb.map(str::to_string);
        let key = key.to_string();
        single_flight(&self.inflight_seasons, key.clone(), async move {
            if let Some(imdb) = imdb.as_deref() {
                arm.seasons_by_imdb(imdb).await
            } else {
                arm.seasons_by_tmdb(tmdb.unwrap_or_default()).await
            }
        })
        .await
    }

    /// The anilist title-search fallback — entries with their season
    /// markers lifted from the `Nth Season` title suffixes.
    async fn anilist_fallback(&self, title: &str) -> Result<Option<Vec<SeasonEntry>>, SourceError> {
        let results = self.anilist_search(title).await?;
        if results.is_empty() {
            return Ok(None);
        }
        let mut entries = Vec::with_capacity(results.len());
        for media in results {
            // Reject unrelated English titles. Romaji-only results retain the
            // existing search fallback because translations cannot be compared literally.
            if media.english.is_some()
                && ![media.romaji.as_deref(), media.english.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|candidate| title_agrees(title, candidate))
            {
                continue;
            }
            let season = season_marker(media.romaji.as_deref())
                .or_else(|| season_marker(media.english.as_deref()))
                .unwrap_or(1);
            entries.push(SeasonEntry {
                anilist_id: media.id,
                mal_id: media.id_mal,
                anidb_id: None,
                imdb: None,
                tmdb_id: None,
                tmdb_season: Some(season),
            });
        }
        Ok((!entries.is_empty()).then_some(entries))
    }
}

/// Conservative alternate-title gate for the search fallback.
fn title_agrees(query: &str, candidate: &str) -> bool {
    let normalize = |title: &str| {
        title
            .chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let query = normalize(query);
    let candidate = normalize(candidate);
    !query.is_empty() && (query == candidate || (query.len() >= 5 && candidate.contains(&query)))
}

/// The ids of the entry covering `season`, when one does.
fn ids_for_season(entries: Option<&[SeasonEntry]>, season: u32) -> Option<SeasonIds> {
    let entries = entries?;
    // OVA/special rows (season 0) never satisfy a normal season request;
    // a movie lookup (season context absent) takes the first entry.
    let entry = entries.iter().find(|entry| entry.matches_season(season))?;
    let anilist_id = entry.anilist_id?;
    Some(SeasonIds {
        anilist_id,
        mal_id: entry.mal_id,
    })
}

/// The `Nth Season` / `Season N` marker of an anilist title, if any.
fn season_marker(title: Option<&str>) -> Option<u32> {
    let lower = title?.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        if *word != "season" {
            continue;
        }
        if let Some(number) = words
            .get(index + 1)
            .and_then(|word| word.parse::<u32>().ok())
        {
            return Some(number);
        }
        if let Some(previous) = index.checked_sub(1).and_then(|index| words.get(index)) {
            let digits: String = previous.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(number) = digits.parse::<u32>() {
                return Some(number);
            }
        }
    }
    None
}

/// Run `load` for `key` so concurrent callers share one in-flight future.
///
/// The entry is removed once the future settles: successes are already in
/// their TTL cache by then, and failures are retried fresh on the next
/// request.
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

/// The shared-load alias for the single-flight helper.
type SharedLoad<T> = Shared<BoxFuture<'static, Result<T, SourceError>>>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::traits::{FetchRequest, FetchResponse};

    /// The captured research fixtures.
    const SWORD_IMDB: &str = r#"[
        {"anidb":16785,"anilist":139587,"myanimelist":49891,"imdb":"tt15483602","themoviedb":134667,"themoviedb-season":1},
        {"anidb":17789,"anilist":159042,"myanimelist":53913,"imdb":"tt15483602","themoviedb":134667,"themoviedb-season":2}
    ]"#;
    const FRIEREN_SEARCH: &str = r#"{"data":{"Page":{"media":[
        {"id":154587,"idMal":52991,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey’s End"}},
        {"id":182255,"idMal":59978,"title":{"romaji":"Sousou no Frieren 2nd Season","english":"Frieren: Beyond Journey’s End Season 2"}}
    ]}}}"#;

    /// A fetcher serving ARM and `AniList` by host, counting arm hits.
    struct MappingMock {
        arm_body: String,
        anilist_body: String,
        arm_calls: AtomicUsize,
        anilist_calls: AtomicUsize,
    }

    #[async_trait]
    impl Fetcher for MappingMock {
        async fn request(
            &self,
            request: FetchRequest,
        ) -> Result<FetchResponse, crate::error::FetchError> {
            let host = request.url.host_str().unwrap_or_default();
            tokio::task::yield_now().await;
            let body = if host.contains("arm") {
                self.arm_calls.fetch_add(1, Ordering::SeqCst);
                self.arm_body.clone()
            } else {
                self.anilist_calls.fetch_add(1, Ordering::SeqCst);
                self.anilist_body.clone()
            };
            Ok(FetchResponse {
                url: request.url,
                status: 200,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    fn service(arm_body: &str, anilist_body: &str) -> (MappingService, Arc<MappingMock>) {
        let mock = Arc::new(MappingMock {
            arm_body: arm_body.to_string(),
            anilist_body: anilist_body.to_string(),
            arm_calls: AtomicUsize::new(0),
            anilist_calls: AtomicUsize::new(0),
        });
        let service = MappingService::new(Arc::clone(&mock) as Arc<dyn Fetcher>);
        (service, mock)
    }

    #[tokio::test]
    async fn resolves_season_ids_from_arm() {
        let (service, mock) = service(SWORD_IMDB, FRIEREN_SEARCH);
        let s1 = service
            .season_ids_by_imdb("tt15483602", 1, Some("Reincarnated as a Sword"))
            .await
            .unwrap_or_else(|e| panic!("arm path must succeed: {e}"))
            .unwrap_or_else(|| panic!("arm must resolve S1"));
        assert_eq!(s1.anilist_id, 139_587);
        assert_eq!(s1.mal_id, Some(49_891));
        let s2 = service
            .season_ids_by_imdb("tt15483602", 2, Some("Reincarnated as a Sword"))
            .await
            .unwrap_or_else(|e| panic!("arm path must succeed: {e}"))
            .unwrap_or_else(|| panic!("arm must resolve S2"));
        assert_eq!(s2.anilist_id, 159_042);
        assert_eq!(s2.mal_id, Some(53_913));
        // One cached arm fetch for both season queries of the same show.
        assert_eq!(mock.arm_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_arm_falls_back_to_anilist_search() {
        let (service, _mock) = service("[]", FRIEREN_SEARCH);
        let ids = service
            .season_ids_by_imdb("tt0000000", 1, Some("Frieren"))
            .await
            .unwrap_or_else(|e| panic!("the fallback must succeed: {e}"))
            .unwrap_or_else(|| panic!("the fallback must resolve"));
        assert_eq!(ids.anilist_id, 154_587);
        assert_eq!(ids.mal_id, Some(52_991));
        let s2 = service
            .season_ids_by_imdb("tt0000000", 2, Some("Frieren"))
            .await
            .unwrap_or_else(|e| panic!("the fallback must succeed: {e}"))
            .unwrap_or_else(|| panic!("the fallback must resolve S2"));
        assert_eq!(s2.anilist_id, 182_255);
    }

    #[tokio::test]
    async fn unknown_everywhere_answers_none() {
        let (service, _mock) = service("[]", r#"{"data":{"Page":{"media":[]}}}"#);
        let ids = service
            .season_ids_by_imdb("tt0000000", 1, Some("Nothing"))
            .await
            .unwrap_or_else(|e| panic!("the miss must not error: {e}"));
        assert_eq!(ids, None);
    }

    #[tokio::test]
    async fn season_zero_needs_an_explicit_row() {
        let (service, _mock) = service(SWORD_IMDB, FRIEREN_SEARCH);
        // OVA rows (season 0) are absent from this fixture; a season-0
        // request must not fall through to the S1 row.
        let ids = service
            .season_ids_by_imdb("tt15483602", 0, Some("Reincarnated as a Sword"))
            .await
            .unwrap_or_else(|e| panic!("the lookup must succeed: {e}"));
        assert_eq!(ids, None);
    }

    #[tokio::test]
    async fn concurrent_lookups_share_one_arm_call() {
        let (service, mock) = service(SWORD_IMDB, FRIEREN_SEARCH);
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let service = service.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .season_ids_by_imdb("tt15483602", 2, Some("Reincarnated as a Sword"))
                    .await
                    .unwrap_or_else(|_| panic!("the shared lookup must resolve"))
                    .unwrap_or_else(|| panic!("arm must resolve S2"))
            }));
        }
        for task in tasks {
            let ids = task.await.unwrap_or_else(|e| panic!("task: {e}"));
            assert_eq!(ids.anilist_id, 159_042);
        }
        assert_eq!(
            mock.arm_calls.load(Ordering::SeqCst),
            1,
            "concurrent lookups must share one arm request"
        );
    }

    #[tokio::test]
    async fn concurrent_canonical_titles_share_one_anilist_call() {
        let (service, mock) = service(
            "[]",
            r#"{"data":{"Media":{"id":159042,"idMal":53913,"title":{"english":"Reincarnated as a Sword Season 2"}}}}"#,
        );
        let tasks = (0..16)
            .map(|_| {
                let service = service.clone();
                tokio::spawn(async move { service.anilist_by_id(159_042).await })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            let media = task
                .await
                .unwrap_or_else(|e| panic!("task: {e}"))
                .unwrap_or_else(|e| panic!("mapping: {e}"));
            assert_eq!(media.and_then(|media| media.id), Some(159_042));
        }
        assert_eq!(mock.anilist_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_arm_uses_the_independent_anilist_fallback() {
        let (service, mock) = service("not JSON", FRIEREN_SEARCH);
        let ids = service
            .season_ids_by_imdb("tt0", 2, Some("Frieren"))
            .await
            .unwrap_or_else(|e| panic!("fallback: {e}"));
        assert_eq!(ids.map(|ids| ids.anilist_id), Some(182_255));
        assert_eq!(mock.arm_calls.load(Ordering::SeqCst), 1);
        // The malformed response must be fetched again, not cached as a miss.
        let _ = service.season_ids_by_imdb("tt0", 2, Some("Frieren")).await;
        assert_eq!(mock.arm_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unrelated_search_results_do_not_become_ids() {
        let (service, _) = service("[]", FRIEREN_SEARCH);
        let ids = service
            .season_ids_by_imdb("tt0000000", 1, Some("Unrelated Show"))
            .await
            .unwrap_or_else(|e| panic!("lookup: {e}"));
        assert_eq!(ids, None);
    }

    #[test]
    fn multi_digit_seasons_and_part_suffixes_are_preserved() {
        assert_eq!(season_marker(Some("Show 12th Season")), Some(12));
        assert_eq!(season_marker(Some("Show Season 10 Part 2")), Some(10));
    }

    #[test]
    fn season_markers_parse_from_titles() {
        assert_eq!(season_marker(Some("Sousou no Frieren 2nd Season")), Some(2));
        assert_eq!(season_marker(Some("Frieren Season 2")), Some(2));
        assert_eq!(season_marker(Some("Sousou no Frieren")), None);
    }
}
