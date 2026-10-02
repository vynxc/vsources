//! The `graphql.anilist.co` public API client (no authentication).
//!
//! `AniList` is the independent fallback for the ARM bridge — a different
//! host, so an ARM outage degrades gracefully — and the id vocabulary the
//! `anichan`-style providers already speak. The lookups the mapping chain
//! needs:
//!
//! - `Media(id: $id)` — the `idMal` cross-reference for an `AniList` id;
//! - `Media(idMal: $id)` — the `AniList` id for a MAL id;
//! - `Page(perPage: 5) { media(search: $search) }` — title search returning
//!   per-season entries with ids (`Sousou no Frieren` and
//!   `… 2nd Season` are separate media with their own ids).
//!
//! Every miss answers `None`/empty; transport failures surface as `Err`
//! and are never cached as misses.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::error::{FetchError, SourceError};
use crate::traits::{FetchRequest, Fetcher};

/// The `AniList` GraphQL endpoint.
const ANILIST_GQL: &str = "https://graphql.anilist.co";
/// TTL for id↔id lookups (static facts).
const MAPPING_TTL: Duration = Duration::from_hours(24);
/// TTL for title searches (details are effectively static).
const SEARCH_TTL: Duration = Duration::from_hours(1);
/// Capacity of the caches.
const MAX_ENTRIES: u64 = 2048;
/// One request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Page size for the title search (the upstream `perPage: 5`).
const SEARCH_PER_PAGE: usize = 5;

/// One `AniList` media entry — the fields the mapping chain reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AniListMedia {
    /// The `AniList` id.
    pub id: Option<u64>,
    /// The `MyAnimeList` cross-reference id.
    pub id_mal: Option<u64>,
    /// The romaji title.
    pub romaji: Option<String>,
    /// The English title.
    pub english: Option<String>,
}

/// A shared `AniList` GraphQL client with TTL caches.
///
/// Cheap to clone; all clones share one cache and one HTTP layer. Construct
/// one per application and hand it to the [`MappingService`](crate::mappings::service::MappingService).
#[derive(Clone)]
pub struct AniListClient {
    inner: Arc<AniListInner>,
}

struct AniListInner {
    fetcher: Arc<dyn Fetcher>,
    /// `anilist:{id}` / `mal:{id}` → media.
    mappings: moka::future::Cache<String, Option<AniListMedia>>,
    /// `search:{query}` → result pages.
    searches: moka::future::Cache<String, Vec<AniListMedia>>,
}

impl AniListClient {
    /// Create a client over the shared fetcher.
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            inner: Arc::new(AniListInner {
                fetcher,
                mappings: moka::future::Cache::builder()
                    .time_to_live(MAPPING_TTL)
                    .max_capacity(MAX_ENTRIES)
                    .build(),
                searches: moka::future::Cache::builder()
                    .time_to_live(SEARCH_TTL)
                    .max_capacity(MAX_ENTRIES)
                    .build(),
            }),
        }
    }

    /// The media entry for an `AniList` id, or `None` when `AniList` does not
    /// know it.
    pub async fn by_id(&self, anilist: u64) -> Result<Option<AniListMedia>, SourceError> {
        let key = format!("anilist:{anilist}");
        if let Some(hit) = self.inner.mappings.get(&key).await {
            return Ok(hit);
        }
        let media = self
            .query_one("query($id: Int) { Media(id: $id, type: ANIME) { id idMal title { romaji english } } }", anilist)
            .await?;
        self.inner.mappings.insert(key, media.clone()).await;
        Ok(media)
    }

    /// The media entry for a MAL id, or `None` when `AniList` has no mapping.
    pub async fn by_id_mal(&self, mal: u64) -> Result<Option<AniListMedia>, SourceError> {
        let key = format!("mal:{mal}");
        if let Some(hit) = self.inner.mappings.get(&key).await {
            return Ok(hit);
        }
        let media = self
            .query_one(
                "query($id: Int) { Media(idMal: $id, type: ANIME) { id idMal title { romaji english } } }",
                mal,
            )
            .await?;
        self.inner.mappings.insert(key, media.clone()).await;
        Ok(media)
    }

    /// Title search — per-season entries with ids, best-first as `AniList`
    /// ranks them (`SEARCH_MATCH, POPULARITY_DESC` upstream).
    pub async fn search(&self, query: &str) -> Result<Vec<AniListMedia>, SourceError> {
        let key = format!("search:{query}");
        if let Some(hit) = self.inner.searches.get(&key).await {
            return Ok(hit);
        }
        let search_query = "query($search: String) { Page(page: 1, perPage: {PER}) { media(type: ANIME, search: $search, sort: [SEARCH_MATCH, POPULARITY_DESC]) { id idMal title { romaji english } } } }"
            .replace("{PER}", &SEARCH_PER_PAGE.to_string());
        let body = serde_json::json!({
            "query": search_query,
            "variables": { "search": query },
        })
        .to_string();
        let response = self.post(body).await?;
        let entries = response
            .data
            .and_then(|data| data.page)
            .and_then(|page| page.media)
            .unwrap_or_default();
        self.inner.searches.insert(key, entries.clone()).await;
        Ok(entries)
    }

    /// One `Media(…)` lookup by numeric id.
    async fn query_one(&self, query: &str, id: u64) -> Result<Option<AniListMedia>, SourceError> {
        let body = serde_json::json!({ "query": query, "variables": { "id": id } }).to_string();
        let response = self.post(body).await?;
        Ok(response.data.and_then(|data| data.media))
    }

    /// One GraphQL POST through the fetcher.
    async fn post(&self, body: String) -> Result<GraphResponse, SourceError> {
        let url = Url::parse(ANILIST_GQL)
            .map_err(|_| SourceError::scrape("anilist", "the `AniList` endpoint must parse"))?;
        let request = FetchRequest::post(url, body)
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json")
            .with_timeout(REQUEST_TIMEOUT);
        let response = self.inner.fetcher.request(request).await?;
        if !response.is_success() {
            return Err(FetchError::Http {
                url: response.url.clone(),
                status: response.status,
            }
            .into());
        }
        response.json::<GraphResponse>().map_err(SourceError::from)
    }
}

/// The GraphQL envelope.
#[derive(Debug, Deserialize)]
struct GraphResponse {
    /// The data block, absent on protocol errors.
    #[serde(default)]
    data: Option<GraphData>,
}

/// The union of the query shapes used above.
#[derive(Debug, Deserialize)]
struct GraphData {
    /// `Media(…)` queries.
    #[serde(default, rename = "Media")]
    media: Option<AniListMedia>,
    /// `Page(…)` queries.
    #[serde(default, rename = "Page")]
    page: Option<GraphPage>,
}

/// The `Page` block.
#[derive(Debug, Deserialize)]
struct GraphPage {
    /// The page's media entries.
    #[serde(default)]
    media: Option<Vec<AniListMedia>>,
}

/// Flatten a media entry's `title { romaji english }`.
impl<'de> Deserialize<'de> for AniListMedia {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// The raw wire shape.
        #[derive(Deserialize)]
        struct RawMedia {
            /// The `AniList` id.
            id: Option<u64>,
            /// The MAL cross-reference.
            #[serde(default, rename = "idMal")]
            id_mal: Option<u64>,
            /// The title block.
            #[serde(default)]
            title: Option<RawTitle>,
        }
        /// The title block.
        #[derive(Deserialize)]
        struct RawTitle {
            /// The romaji title.
            #[serde(default)]
            romaji: Option<String>,
            /// The English title.
            #[serde(default)]
            english: Option<String>,
        }
        let raw = RawMedia::deserialize(deserializer)?;
        let romaji = raw.title.as_ref().and_then(|title| title.romaji.clone());
        let english = raw.title.and_then(|title| title.english);
        Ok(AniListMedia {
            id: raw.id,
            id_mal: raw.id_mal,
            romaji,
            english,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;

    use super::*;
    use crate::traits::FetchResponse;

    /// A fetcher serving one canned GraphQL body.
    struct MockAniList {
        body: String,
    }

    #[async_trait]
    impl Fetcher for MockAniList {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            Ok(FetchResponse {
                url: request.url,
                status: 200,
                headers: BTreeMap::new(),
                body: self.body.clone(),
            })
        }
    }

    /// The captured research fixture: the Frieren search page.
    const FRIEREN_SEARCH: &str = r#"{"data":{"Page":{"media":[
        {"id":154587,"idMal":52991,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey’s End"}},
        {"id":209939,"idMal":63816,"title":{"romaji":"Sousou no Frieren 3rd Season"}},
        {"id":182255,"idMal":59978,"title":{"romaji":"Sousou no Frieren 2nd Season","english":"Frieren: Beyond Journey’s End Season 2"}}
    ]}}}"#;

    #[tokio::test]
    async fn search_returns_per_season_entries() {
        let client = AniListClient::new(Arc::new(MockAniList {
            body: FRIEREN_SEARCH.to_string(),
        }));
        let entries = client
            .search("Frieren")
            .await
            .unwrap_or_else(|e| panic!("anilist search must succeed: {e}"));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, Some(154_587));
        assert_eq!(entries[0].id_mal, Some(52_991));
        assert_eq!(
            entries[0].english.as_deref(),
            Some("Frieren: Beyond Journey’s End")
        );
        assert_eq!(entries[2].id, Some(182_255));
        assert_eq!(
            entries[1].romaji.as_deref(),
            Some("Sousou no Frieren 3rd Season")
        );
    }

    #[tokio::test]
    async fn id_lookups_parse_the_media_block() {
        let client = AniListClient::new(Arc::new(MockAniList {
            body: r#"{"data":{"Media":{"id":139587,"idMal":49891,"title":{"romaji":"Tensei Shitara Ken Deshita","english":"Reincarnated as a Sword"}}}}"#
                .to_string(),
        }));
        let media = client
            .by_id(139_587)
            .await
            .unwrap_or_else(|e| panic!("anilist lookup must succeed: {e}"))
            .unwrap_or_else(|| panic!("anilist must know the id"));
        assert_eq!(media.id, Some(139_587));
        assert_eq!(media.id_mal, Some(49_891));
        assert_eq!(media.english.as_deref(), Some("Reincarnated as a Sword"));
    }
}
