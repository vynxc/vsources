//! Minimal Stremio Cinemeta client for the TUI example.
//!
//! Uses `https://v3-cinemeta.strem.io/manifest.json`'s search-supporting
//! catalog (`catalog/{movie,series}/top`) — no TMDB key required. The
//! returned metas are `IMDb` ids (`tt…`), which `MediaId::parse` accepts
//! directly, so a selected hit can be resolved by the engine without a
//! manual id step.

use std::sync::Arc;
use std::time::{Duration, Instant};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use tokio::sync::oneshot;
use vsources::{Fetcher, MediaId, MediaRef, MediaType};
use vsources_core::traits::FetchRequest;

/// Cinemeta base URL — the manifest lives at `{base}/manifest.json`.
const CINEMETA_BASE: &str = "https://v3-cinemeta.strem.io";
/// How long a completed search stays "fresh" before being re-run on Enter.
const SEARCH_TTL: Duration = Duration::from_secs(300);

/// One `metas` entry: the bits the result list needs.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// `IMDb` id (Cinemeta's catalog ids are `tt…`).
    pub imdb_id: String,
    /// Display title.
    pub name: String,
    /// Release info: a year, a year range, or occasionally a date.
    pub release: String,
    /// `movie` or `series` — matches the searched catalog.
    pub kind: MediaType,
}

#[derive(Deserialize)]
struct CatalogResponse {
    metas: Vec<MetaEntry>,
}

#[derive(Deserialize)]
struct MetaEntry {
    id: String,
    name: String,
    #[serde(default)]
    #[serde(rename = "releaseInfo")]
    release_info: Option<String>,
}

/// Where the search UI stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchState {
    /// No query typed yet.
    Idle,
    /// A search is in flight.
    Loading,
    /// Results are displayed.
    Done,
}

/// The search half of the TUI's query form.
pub struct Search {
    /// The typed query.
    pub query: String,
    /// The current hits, in Cinemeta's relevance order.
    pub hits: Vec<SearchHit>,
    /// The selected hit (also driven by the table-style navigation).
    pub selection: Option<usize>,
    /// Which catalog the current hits came from.
    pub hits_kind: MediaType,
    /// UI state.
    pub state: SearchState,
    /// When the current hits landed (TTL guard for Enter re-runs).
    fetched_at: Option<Instant>,
    /// The pending in-flight search's completion, if any.
    pending: Option<oneshot::Receiver<Result<Vec<SearchHit>, String>>>,
}

impl Default for Search {
    /// Empty, idle.
    fn default() -> Self {
        Self::new()
    }
}

impl Search {
    /// Empty, idle.
    pub fn new() -> Self {
        Self {
            query: String::new(),
            hits: Vec::new(),
            selection: None,
            hits_kind: MediaType::Movie,
            state: SearchState::Idle,
            fetched_at: None,
            pending: None,
        }
    }

    /// Kick off a search for the typed query (no-op while one is in flight).
    ///
    /// `fetcher` is the engine's own fetcher when available; a bare
    /// `ChromeFetcher` otherwise. The response arrives through
    /// [`Self::poll`].
    pub fn start(&mut self, fetcher: Arc<dyn Fetcher>) {
        if self.state == SearchState::Loading {
            return;
        }
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.state = SearchState::Idle;
            self.hits.clear();
            self.selection = None;
            return;
        }
        self.state = SearchState::Loading;
        let kind = self.hits_kind;
        let (sender, receiver) = oneshot::channel();
        self.pending = Some(receiver);
        tokio::spawn(run_search(fetcher, query, kind, sender));
    }

    /// Clear results and detach any older in-flight request after an edit.
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.hits.clear();
        self.selection = None;
        self.state = SearchState::Idle;
        self.fetched_at = None;
    }

    /// Change the target catalog (movie/series toggle) and drop stale hits.
    pub fn set_kind(&mut self, kind: MediaType) {
        if self.hits_kind != kind {
            self.hits_kind = kind;
            self.pending = None;
            self.hits.clear();
            self.selection = None;
            self.state = SearchState::Idle;
            self.fetched_at = None;
        }
    }

    /// Drain the in-flight search's completion, when it has landed.
    ///
    /// Returns the status-line message the UI should show, if any.
    pub fn poll(&mut self) -> Option<String> {
        let receiver = self.pending.as_mut()?;
        match receiver.try_recv() {
            Ok(Ok(hits)) => {
                self.pending = None;
                self.state = SearchState::Done;
                self.fetched_at = Some(Instant::now());
                self.hits = hits;
                self.selection = (!self.hits.is_empty()).then_some(0);
                Some(format!("{} results", self.hits.len()))
            }
            Ok(Err(error)) => {
                self.pending = None;
                self.state = SearchState::Done;
                self.hits.clear();
                self.selection = None;
                Some(format!("search failed: {error}"))
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending = None;
                self.state = SearchState::Idle;
                Some("the search task died".to_string())
            }
        }
    }

    /// Mark fixture results as freshly received.
    #[cfg(test)]
    pub fn mark_fresh(&mut self) {
        self.fetched_at = Some(Instant::now());
    }

    /// The selected hit, if any.
    pub fn selected(&self) -> Option<&SearchHit> {
        self.selection.and_then(|index| self.hits.get(index))
    }

    /// Move the result selection by `delta` (clamped, wrapping at ends).
    pub fn step_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.selection = None;
            return;
        }
        let current = self.selection.unwrap_or(0).cast_signed();
        let max = self.hits.len().cast_signed() - 1;
        let next = (current + delta).clamp(0, max);
        self.selection = Some(next.cast_unsigned());
    }

    /// Whether the current hits can still be trusted for an Enter resolve.
    fn fresh(&self) -> bool {
        self.fetched_at.is_some_and(|at| at.elapsed() < SEARCH_TTL)
    }

    /// The media reference the selection describes, or a reason string.
    ///
    /// Re-runs a stale search when the query and hits have drifted; the
    /// caller surfaces the status and the user presses Enter again.
    pub fn media(&self) -> Result<MediaRefLite, String> {
        let Some(hit) = self.selected() else {
            return Err("no search result selected — type a title and Enter".to_string());
        };
        if !self.fresh() {
            return Err("results are stale — press Enter to search again".to_string());
        }
        Ok(MediaRefLite {
            imdb_id: hit.imdb_id.clone(),
            kind: hit.kind,
        })
    }
}

/// The resolve request derived from a search hit.
pub struct MediaRefLite {
    /// The `tt…` id.
    pub imdb_id: String,
    /// movie or series.
    pub kind: MediaType,
}

impl MediaRefLite {
    /// Convert to the engine's `MediaRef` with optional season/episode.
    pub fn into_media_ref(self, season: Option<u32>, episode: Option<u32>) -> MediaRef {
        MediaRef {
            id: MediaId::parse(&self.imdb_id).unwrap_or(MediaId::Imdb(self.imdb_id)),
            kind: self.kind,
            season,
            episode,
        }
    }
}

/// One catalog search: `GET /catalog/{type}/top/search={query}.json`.
async fn run_search(
    fetcher: Arc<dyn Fetcher>,
    query: String,
    kind: MediaType,
    done: oneshot::Sender<Result<Vec<SearchHit>, String>>,
) {
    let type_path = match kind {
        MediaType::Series => "series",
        MediaType::Movie => "movie",
    };
    // The manifest's `top` catalog declares `search` support for both
    // types; this is the documented addon catalog route.
    let encoded: String = utf8_percent_encode(&query, NON_ALPHANUMERIC).to_string();
    let url = format!("{CINEMETA_BASE}/catalog/{type_path}/top/search={encoded}.json");
    let Ok(url) = url::Url::parse(&url) else {
        let _ = done.send(Err(format!("bad search url: {url}")));
        return;
    };
    let request = FetchRequest::get(url).with_timeout(Duration::from_secs(10));
    let outcome = fetcher
        .request(request)
        .await
        .map_err(|error| format!("cinemeta: {error}"));
    let result = match outcome {
        Ok(response) if response.status >= 200 && response.status < 300 => {
            serde_json::from_str::<CatalogResponse>(&response.body)
                .map(|catalog| {
                    catalog
                        .metas
                        .into_iter()
                        .map(|meta| SearchHit {
                            imdb_id: meta.id,
                            name: meta.name,
                            release: meta.release_info.unwrap_or_default(),
                            kind,
                        })
                        .collect::<Vec<_>>()
                })
                .map_err(|error| format!("cinemeta json: {error}"))
        }
        Ok(response) => Err(format!("cinemeta returned HTTP {}", response.status)),
        Err(error) => Err(error),
    };
    let _ = done.send(result);
}

/// One episode retained for the picker and identity verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// TMDB/Stremio season number, including season zero specials.
    pub season: u32,
    /// Episode number within the season.
    pub episode: u32,
    /// Episode title.
    pub title: String,
    /// ISO release/air date, when supplied.
    pub released: Option<String>,
}

#[derive(Deserialize)]
struct SeriesResponse {
    meta: Option<SeriesMeta>,
}
#[derive(Deserialize)]
struct SeriesMeta {
    id: String,
    #[serde(default)]
    videos: Vec<Video>,
}
#[derive(Deserialize)]
struct Video {
    #[serde(default)]
    season: Option<u32>,
    #[serde(default)]
    episode: Option<u32>,
    #[serde(default, alias = "name")]
    title: String,
    #[serde(default)]
    released: Option<String>,
}

/// A series episode picker. Replacing the receiver discards stale responses.
#[derive(Default)]
pub struct Episodes {
    /// Series identity that owns the list.
    pub imdb_id: Option<String>,
    /// Ordered by season and episode, retaining titles and dates.
    pub items: Vec<Episode>,
    /// Row selection.
    pub selection: Option<usize>,
    /// Whether metadata is in flight.
    pub loading: bool,
    pending: Option<oneshot::Receiver<Result<Vec<Episode>, String>>>,
}

impl Episodes {
    /// Fetch a selected series' metadata without blocking input.
    pub fn start(&mut self, fetcher: Arc<dyn Fetcher>, imdb_id: String) {
        *self = Self {
            imdb_id: Some(imdb_id.clone()),
            loading: true,
            ..Self::default()
        };
        let (sender, receiver) = oneshot::channel();
        self.pending = Some(receiver);
        tokio::spawn(async move {
            let _ = sender.send(fetch_episodes(fetcher.as_ref(), &imdb_id).await);
        });
    }

    /// Accept the current request only.
    pub fn poll(&mut self) -> Option<String> {
        let outcome = match self.pending.as_mut()?.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return None,
            Err(oneshot::error::TryRecvError::Closed) => Err("episode task stopped".into()),
        };
        self.pending = None;
        self.loading = false;
        match outcome {
            Ok(items) => {
                self.items = items;
                // Normal episodes first; specials remain selectable.
                self.selection = self
                    .items
                    .iter()
                    .position(|ep| ep.season > 0)
                    .or_else(|| (!self.items.is_empty()).then_some(0));
                Some(format!(
                    "{} episodes — select an episode and Enter",
                    self.items.len()
                ))
            }
            Err(error) => Some(format!("episodes failed: {error}")),
        }
    }

    /// Move selection within the list.
    pub fn step(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let current = self.selection.unwrap_or(0).cast_signed();
        self.selection = Some(
            (current + delta)
                .clamp(0, self.items.len().cast_signed() - 1)
                .cast_unsigned(),
        );
    }

    /// The selected episode.
    pub fn selected(&self) -> Option<&Episode> {
        self.items.get(self.selection?)
    }
}

/// Load metadata for exactly the requested series identity.
pub async fn fetch_episodes(fetcher: &dyn Fetcher, imdb_id: &str) -> Result<Vec<Episode>, String> {
    let url = url::Url::parse(&format!("{CINEMETA_BASE}/meta/series/{imdb_id}.json"))
        .map_err(|error| error.to_string())?;
    let response = fetcher
        .request(FetchRequest::get(url).with_timeout(Duration::from_secs(10)))
        .await
        .map_err(|error| error.to_string())?;
    if !response.is_success() {
        return Err(format!("HTTP {}", response.status));
    }
    parse_episodes(&response.body, imdb_id)
}

fn parse_episodes(body: &str, imdb_id: &str) -> Result<Vec<Episode>, String> {
    let response: SeriesResponse = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let meta = response.meta.ok_or("missing series metadata")?;
    if meta.id != imdb_id {
        return Err("series identity mismatch".into());
    }
    let mut items: Vec<_> = meta
        .videos
        .into_iter()
        .filter_map(|video| {
            Some(Episode {
                season: video.season?,
                episode: video.episode?,
                title: video.title,
                released: video.released,
            })
        })
        .collect();
    items.sort_by_key(|ep| (ep.season, ep.episode));
    items.dedup_by_key(|ep| (ep.season, ep.episode));
    Ok(items)
}

/// A stub `main`: this file is a module of the `tui` example
/// (`mod cinemeta;` there), but cargo also discovers it as its own
/// example target — running it directly prints where the real entry
/// point lives. Its tests still run under `cargo test --example cinemeta`.
/// The allow is for the module inclusion (dead there) while the standalone
/// target keeps a runnable entry point.
#[allow(dead_code)]
fn main() {
    println!("cinemeta.rs is the TUI example's search module; run the tui example instead:");
    println!("  cargo run -p vsources --example tui");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_catalog_response_shapes() {
        let body = r#"{
            "metas": [
                {"id":"tt31076325","name":"Frieren","type":"movie","releaseInfo":"1996"},
                {"id":"tt0343937","name":"No Release"}
            ]
        }"#;
        let catalog: CatalogResponse = serde_json::from_str(body).unwrap();
        assert_eq!(catalog.metas.len(), 2);
        assert_eq!(catalog.metas[0].id, "tt31076325");
        assert_eq!(catalog.metas[0].release_info.as_deref(), Some("1996"));
        assert_eq!(catalog.metas[1].release_info, None);
    }

    #[test]
    fn episode_metadata_is_grouped_and_keeps_titles_and_air_dates() {
        let items = parse_episodes(
            r#"{"meta":{"id":"tt1234567","videos":[
            {"season":2,"episode":2,"title":"Return","released":"2026-01-02T00:00:00.000Z"},
            {"season":1,"episode":1,"name":"Beginning"},
            {"season":0,"episode":1,"title":"Special"},
            {"season":2,"episode":1,"title":"Second Beginning"},
            {"title":"Trailer"},
            {"season":2,"episode":1,"title":"Duplicate"}
        ]}}"#,
            "tt1234567",
        )
        .unwrap_or_else(|e| panic!("metadata: {e}"));
        assert_eq!(
            items
                .iter()
                .map(|ep| (ep.season, ep.episode))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 1), (2, 1), (2, 2)]
        );
        assert_eq!(items[1].title, "Beginning");
        assert_eq!(items[3].title, "Return");
        assert_eq!(
            items[3].released.as_deref(),
            Some("2026-01-02T00:00:00.000Z")
        );
        assert!(parse_episodes(r#"{"meta":{"id":"ttOTHER","videos":[]}}"#, "tt1234567").is_err());
    }

    #[test]
    fn switching_catalog_discards_inflight_old_results() {
        let mut search = Search::new();
        let (sender, receiver) = oneshot::channel();
        search.pending = Some(receiver);
        search.state = SearchState::Loading;
        search.set_kind(MediaType::Series);
        assert!(sender.send(Ok(Vec::new())).is_err());
        assert!(search.poll().is_none());
        assert_eq!(search.hits_kind, MediaType::Series);
        assert_eq!(search.state, SearchState::Idle);
    }

    #[test]
    fn media_ref_from_hit_keeps_id_kind_and_episode() {
        let hit = MediaRefLite {
            imdb_id: "tt209867".to_string(),
            kind: MediaType::Series,
        };
        let media = hit.into_media_ref(Some(1), Some(2));
        assert_eq!(media.id, MediaId::Imdb("tt209867".to_string()));
        assert_eq!(media.kind, MediaType::Series);
        assert_eq!(media.season, Some(1));
        assert_eq!(media.episode, Some(2));
    }
}
