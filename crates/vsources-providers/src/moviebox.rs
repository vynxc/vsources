//! `MovieBox`: mobile DASH and legacy web MP4s from the aoneroom JSON APIs.
//!
//! The default catalog enables the current mobile API when
//! `MOVIEBOX_MOBILE_SIGNING_KEY` contains a hex signing key. Embedders can use
//! [`MovieBox::with_mobile_signing_key`] instead. Anonymous visitor sessions
//! yield signed CDN cookies; the returned DASH stream carries those cookies
//! and the required Android User-Agent. Keys are never bundled. Without a
//! key, only the legacy web flow below is used.
//!
//! Ports `src/source/MovieBox.js` (`movie-box.co` — movies, series,
//! anime, K-drama with direct MP4 URLs on the hakunaymatata CDN). A
//! clean JSON flow, no scraping:
//!
//! 1. An anonymous JWT: `POST /subject/search-suggest` hands it out in
//!    the `x-user` response header (auto-issued, ~90-day validity
//!    upstream; the port refreshes it hourly like the upstream
//!    `JWT_TTL`).
//! 2. Search: `POST /subject/search` with up to three title variants
//!    (raw, diacritics-folded, punctuation-stripped), scoring items by
//!    normalized containment plus a year bonus, keeping the best match
//!    at ≥ 60.
//! 3. Play: `GET /subject/play?subjectId=…&se=…&ep=…&detailPath=…&streamSignType=1`
//!    → `data.streams` (direct MP4s, ~1h-signed — hence the short
//!    stream TTL). The endpoint can answer empty without a `Referer`
//!    (upstream's own comment), so the port retries with
//!    `Referer: https://movie-box.co/movies/{detailPath}` exactly like
//!    the upstream second fetch.
//!
//! The `Authorization: Bearer`, `X-Client-Info: {"timezone":"UTC"}` and
//! `X-Request-Lang: en` headers ride every API call; the hakunaymatata
//! CDN hotlinks gate on `Referer: https://movie-box.co/` (upstream
//! routed that through its `/proxy`; the port ships the header on
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! instead).
//!
//! Cuts for the library port:
//!
//! - The upstream fired the API through `got-scraping` directly with a
//!   hand-set Chrome UA; the port sends the same header set through the
//!   context fetcher, which already impersonates a browser.
//! - `meta.title` has no `StreamMeta` field — the stream label carries
//!   the `${title} (${resolutions}p)` form.
//! - Result caching and the JWT's cross-request sharing beyond one
//!   provider instance are the parent's `CachedSource` domain; the
//!   in-instance JWT cache is kept because it is load-bearing (every
//!   API call needs it).
//! - The upstream `got` GOAWAY-retry logic is the net layer's concern.

mod mobile;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

/// The aoneroom API root.
const API_BASE: &str = "https://h5-api.aoneroom.com/wefeed-h5api-bff";
/// The site the API and CDN hotlinks gate on.
const SITE_BASE: &str = "https://movie-box.co";
/// The anonymous JWT refresh window (upstream `JWT_TTL`, deliberately far
/// below the ~90-day server validity).
const JWT_TTL: Duration = Duration::from_hours(1);
/// Every API call's timeout (upstream: 15s).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Stream URLs are time-limited (~1h upstream), so results die fast.
const TTL: Duration = Duration::from_mins(10);

/// The `MovieBox` provider.
pub struct MovieBox {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The anonymous JWT and when it was minted.
    jwt: Mutex<Option<(String, Instant)>>,
    /// Optional current mobile API, configured with a runtime signing key.
    mobile: Option<mobile::Client>,
}

impl MovieBox {
    /// A provider over the shared TMDB client.
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "moviebox".to_string(),
                label: "MovieBox".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Some(
                    Url::parse("https://movie-box.co")
                        .unwrap_or_else(|e| panic!("valid MovieBox base URL: {e}")),
                ),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            jwt: Mutex::new(None),
            mobile: None,
        }
    }

    /// Apply the default catalog's optional environment configuration.
    pub(crate) fn with_environment_mobile_key(mut self) -> Self {
        self.mobile = mobile::Client::from_env();
        self
    }

    /// Enable the current mobile DASH API with a caller-supplied signing key.
    /// The equivalent environment setting is `MOVIEBOX_MOBILE_SIGNING_KEY` (hex).
    #[must_use]
    pub fn with_mobile_signing_key(mut self, key: Vec<u8>) -> Self {
        self.mobile = Some(mobile::Client::new(key));
        self
    }

    /// The anonymous JWT, minting a fresh one when the cache is empty or
    /// stale. Upstream has the same benign mint race (no lock around the
    /// fetch). A dead endpoint is a miss (upstream's `null` JWT folds
    /// into an empty result); a 200 without a parsable `x-user` header
    /// is a structural surprise.
    async fn jwt(&self, ctx: &ResolveCtx<'_>) -> Result<String, SourceError> {
        {
            let cached = self
                .jwt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((token, minted)) = cached.as_ref()
                && minted.elapsed() < JWT_TTL
            {
                return Ok(token.clone());
            }
        }

        let url = Url::parse(&format!("{API_BASE}/subject/search-suggest"))
            .map_err(|_| SourceError::scrape("moviebox", "invalid search-suggest URL"))?;
        let request = FetchRequest::post(url, r#"{"keyword":"a","perPage":1}"#.to_string())
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json")
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx
            .fetcher
            .request(request)
            .await
            .map_err(|_| SourceError::NotFound)?;
        if !response.is_success() {
            return Err(SourceError::NotFound);
        }
        let header = response.header("x-user").ok_or_else(|| {
            SourceError::scrape("moviebox", "search-suggest 200 without an x-user JWT")
        })?;
        let user: JwtEnvelope = serde_json::from_str(header).map_err(|_| {
            SourceError::scrape("moviebox", "search-suggest x-user is not a JWT envelope")
        })?;
        let token = user.token;
        *self
            .jwt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// `POST` an API path and parse the JSON body (upstream `apiPost`).
    async fn api_post(
        &self,
        ctx: &ResolveCtx<'_>,
        jwt: &str,
        path: &str,
        body: &str,
    ) -> Option<serde_json::Value> {
        let url = Url::parse(&format!("{API_BASE}{path}")).ok()?;
        let request = FetchRequest::post(url, body.to_string())
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json")
            .with_header("Authorization", format!("Bearer {jwt}"))
            .with_header("X-Client-Info", r#"{"timezone":"UTC"}"#)
            .with_header("X-Request-Lang", "en")
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        serde_json::from_str(&response.body).ok()
    }

    /// `GET` an API path and parse the JSON body (upstream `apiGet`),
    /// optionally with a `Referer`.
    async fn api_get(
        &self,
        ctx: &ResolveCtx<'_>,
        jwt: &str,
        path: &str,
        referer: Option<&str>,
    ) -> Option<serde_json::Value> {
        let url = Url::parse(&format!("{API_BASE}{path}")).ok()?;
        let mut request = FetchRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("Authorization", format!("Bearer {jwt}"))
            .with_header("X-Client-Info", r#"{"timezone":"UTC"}"#)
            .with_header("X-Request-Lang", "en")
            .with_timeout(REQUEST_TIMEOUT);
        if let Some(referer) = referer {
            request = request.with_header("Referer", referer);
        }
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        serde_json::from_str(&response.body).ok()
    }

    /// Search by title and return the best matching item at score ≥ 60
    /// (upstream `findItem`).
    async fn find_item(
        &self,
        ctx: &ResolveCtx<'_>,
        jwt: &str,
        name: &str,
        year: Option<u16>,
        subject_type: u32,
    ) -> Option<SearchItem> {
        let name_norm = normalize(name);
        let stripped = strip_punctuation(name);
        let queries: Vec<String> = [name.to_string(), fold_diacritics(name), stripped]
            .into_iter()
            .filter(|query| !query.is_empty())
            .fold(Vec::new(), |mut acc, query| {
                if !acc.contains(&query) {
                    acc.push(query);
                }
                acc
            });

        for query in &queries {
            let body = serde_json::json!({
                "keyword": query,
                "page": 1,
                "perPage": 20,
                "subjectType": subject_type,
            })
            .to_string();
            let data = self.api_post(ctx, jwt, "/subject/search", &body).await?;
            let payload: SearchResponse = serde_json::from_value(data).ok()?;
            let Some(items) = payload.data.map(|data| data.items) else {
                continue;
            };

            let mut best: Option<(u32, SearchItem)> = None;
            for item in items {
                if item.subject_type != subject_type {
                    continue;
                }
                let item_norm = normalize(&item.title);
                if item_norm.is_empty() {
                    continue;
                }

                let mut score = if item_norm == name_norm {
                    100
                } else if item_norm.contains(&name_norm) || name_norm.contains(&item_norm) {
                    // `min/max * 90`, exact in integer math.
                    let min_len = item_norm.len().min(name_norm.len());
                    let max_len = item_norm.len().max(name_norm.len());
                    u32::try_from(min_len)
                        .ok()
                        .and_then(|min| u32::try_from(max_len).ok().map(|max| min * 90 / max))
                        .unwrap_or(0)
                } else {
                    0
                };

                // Year bonus — helps distinguish remakes/sequels.
                if score > 0
                    && let Some(year) = year
                    && let Some(release) = &item.release_date
                    && let Some(item_year) = release.get(..4)
                    && let Ok(item_year) = item_year.parse::<u16>()
                    && item_year == year
                {
                    score += 10;
                }

                if score > best.as_ref().map_or(0, |(score, _)| *score) {
                    best = Some((score, item));
                }
            }

            if let Some((score, item)) = best
                && score >= 60
            {
                return Some(item);
            }
        }
        None
    }

    /// The play endpoint's streams, retrying once with the detail-page
    /// `Referer` like the upstream fallback fetch.
    async fn play_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        jwt: &str,
        media: &MediaRef,
        item: &SearchItem,
    ) -> Option<Vec<PlayStream>> {
        let se = media.season.unwrap_or(0);
        let ep = media.episode.unwrap_or(0);
        let detail = item.detail_path.as_deref().unwrap_or_default();
        let path = format!(
            "/subject/play?subjectId={}&se={se}&ep={ep}&detailPath={detail}&streamSignType=1",
            item.subject_id
        );

        let mut payload = self.api_get(ctx, jwt, &path, None).await;
        let has_streams = payload.as_ref().is_some_and(|value| {
            value
                .get("data")
                .and_then(|data| data.get("streams"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|streams| !streams.is_empty())
        });
        if !has_streams {
            payload = self
                .api_get(
                    ctx,
                    jwt,
                    &path,
                    Some(&format!("{SITE_BASE}/movies/{detail}")),
                )
                .await;
        }

        let parsed: PlayResponse = serde_json::from_value(payload?).ok()?;
        let streams = parsed.data.map(|data| data.streams)?;
        (!streams.is_empty()).then_some(streams)
    }
}

#[async_trait]
impl Source for MovieBox {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(&self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;

        if let Some(mobile) = &self.mobile
            && let Some(streams) = mobile.resolve(ctx, media, &name, year).await
            && !streams.is_empty()
        {
            return Ok(streams);
        }

        // `subjectType`: 1 = movie, 2 = series.
        let subject_type = if media.season.is_some() { 2 } else { 1 };
        let jwt = self.jwt(ctx).await?;
        let item = self
            .find_item(ctx, &jwt, &name, year, subject_type)
            .await
            .ok_or(SourceError::NotFound)?;
        let play = self
            .play_streams(ctx, &jwt, media, &item)
            .await
            .ok_or(SourceError::NotFound)?;

        let title = display_title(&name, year, media.season, media.episode);
        let streams = build_streams(play, &title);
        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(streams)
    }
}

/// The TMDB id for the reference (IMDb-keyed references resolve through
/// `/find`); TMDB miss pages map to [`SourceError::NotFound`] like the
/// upstream `NotFoundError`.
async fn tmdb_id(tmdb: &TmdbClient, media: &MediaRef) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
    }
}

/// The media name and year, preferring pre-resolved context metadata
/// (the upstream resolver resolved TMDB before calling sources) and
/// falling back to the shared client.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = &ctx.media
        && !resolved.name.is_empty()
    {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
    Ok((name.name, name.year))
}

/// Map miss-shaped failures onto the not-found answer.
fn soften<T>(error: Result<T, SourceError>) -> Result<T, SourceError> {
    match error {
        Err(
            SourceError::NotFound
            | SourceError::Fetch(FetchError::NotFound { .. } | FetchError::Http { status: 404, .. }),
        ) => Err(SourceError::NotFound),
        other => other,
    }
}

/// The upstream display title: name + `S01E02` for episodes, name +
/// ` (year)` for movies.
fn display_title(
    name: &str,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
) -> String {
    if season.is_some() {
        format!(
            "{name} S{:02}E{:02}",
            season.unwrap_or(1),
            episode.unwrap_or(1)
        )
    } else {
        let year = year.map(|y| y.to_string()).unwrap_or_default();
        format!("{name} ({year})")
    }
}

/// The `x-user` header of `search-suggest`.
#[derive(Deserialize)]
struct JwtEnvelope {
    /// The anonymous bearer token.
    token: String,
}

/// The `/subject/search` envelope.
#[derive(Deserialize)]
struct SearchResponse {
    /// The search payload.
    data: Option<SearchData>,
}

/// The search payload.
#[derive(Deserialize)]
struct SearchData {
    /// The matched subjects.
    #[serde(default)]
    items: Vec<SearchItem>,
}

/// One search hit.
#[derive(Deserialize, Clone)]
struct SearchItem {
    /// The subject's id (a large number as a string).
    #[serde(rename = "subjectId")]
    subject_id: String,
    /// 1 = movie, 2 = series.
    #[serde(rename = "subjectType")]
    subject_type: u32,
    /// The subject's title.
    title: String,
    /// The release date (`YYYY-MM-DD`).
    #[serde(rename = "releaseDate", default)]
    release_date: Option<String>,
    /// The detail path (`/movies/{detailPath}`).
    #[serde(rename = "detailPath", default)]
    detail_path: Option<String>,
}

/// The `/subject/play` envelope.
#[derive(Deserialize)]
struct PlayResponse {
    /// The play payload.
    data: Option<PlayData>,
}

/// The play payload.
#[derive(Deserialize)]
struct PlayData {
    /// The playable direct streams.
    #[serde(default)]
    streams: Vec<PlayStream>,
}

/// One play stream.
#[derive(Deserialize)]
struct PlayStream {
    /// The direct MP4 URL.
    #[serde(default)]
    url: Option<String>,
    /// The vertical resolution label (e.g. `1080`).
    #[serde(default)]
    resolutions: Option<String>,
    /// Whether the stream needs a VIP subscription.
    #[serde(rename = "vipLocked", default)]
    vip_locked: Option<bool>,
}

/// Build the stream list (upstream `buildStreams`): skip VIP-locked and
/// url-less entries, dedupe URLs, hotlink-gate the hakunaymatata CDN.
fn build_streams(play: Vec<PlayStream>, title: &str) -> Vec<Stream> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for entry in play {
        let Some(url_text) = entry.url.filter(|url| !url.is_empty()) else {
            continue;
        };
        if entry.vip_locked.unwrap_or(false) {
            continue;
        }
        if !seen.insert(url_text.clone()) {
            continue;
        }
        let Ok(url) = Url::parse(&url_text) else {
            continue;
        };

        // The retired web API can return a short app-upgrade announcement.
        // It is valid video, but it is not the requested movie or episode.
        if url.host_str() == Some("macdn.aoneroom.com") && url.path().starts_with("/other/") {
            continue;
        }

        let height: Option<u16> = entry
            .resolutions
            .as_deref()
            .and_then(|resolutions| resolutions.parse().ok());

        let mut stream = Stream::new(url.clone(), Format::Mp4).with_ttl(TTL);
        stream.label = Some(format!(
            "{title} ({}p)",
            entry.resolutions.as_deref().unwrap_or_default()
        ));
        stream.meta.resolution = height;
        stream.meta.languages = vec![CountryCode::Multi];
        stream.meta.source_id = Some("moviebox".to_string());
        stream.meta.source_label = Some("MovieBox".to_string());
        // The hakunaymatata CDN 429s hotlinks without the site Referer.
        if url
            .host_str()
            .is_some_and(|host| host.ends_with(".hakunaymatata.com"))
        {
            stream = stream.with_referer(format!("{SITE_BASE}/"));
        }
        out.push(stream);
    }
    out
}

/// The upstream normalizer: lowercase, fold diacritics (NFD + combining
/// strip upstream — a Latin-1 table here, full Unicode normalization
/// would need a dependency), drop `[^a-z0-9\s]`, collapse whitespace.
fn normalize(value: &str) -> String {
    fold_diacritics(&value.to_lowercase())
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == ' ')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fold the common Latin-1 accented letters to their base letters.
fn fold_diacritics(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ý' | 'ÿ' => 'y',
            other => other,
        })
        .collect()
}

/// The upstream's third query variant: non-alphanumerics become spaces.
fn strip_punctuation(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == ' ' {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;
    use vsources_core::traits::ResolvedMedia;
    use vsources_core::traits::{FetchResponse, Fetcher};

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        status: u16,
        body: String,
        headers: BTreeMap<String, String>,
    }

    impl Scripted {
        /// A 200 JSON body.
        fn json(value: &serde_json::Value) -> Self {
            Self::json_with_headers(value, BTreeMap::new())
        }

        /// A 200 JSON body plus response headers.
        fn json_with_headers(value: &serde_json::Value, headers: BTreeMap<String, String>) -> Self {
            Self {
                status: 200,
                body: value.to_string(),
                headers,
            }
        }
    }

    /// A fetcher serving scripted pages by host+path (in order, the last
    /// repeating) and recording every request it sees.
    struct MockFetcher {
        pages: Mutex<HashMap<String, VecDeque<Scripted>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl MockFetcher {
        /// A fetcher serving nothing yet.
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Serve `key` (host + path) with `response`.
        fn serve(self, key: &str, response: Scripted) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.to_string())
                .or_default()
                .push_back(response);
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Every request sent to `key` (host + path), in order.
        fn requests_to(&self, key: &str) -> Vec<FetchRequest> {
            self.requests()
                .into_iter()
                .filter(|request| {
                    let host = request.url.host_str().unwrap_or_default();
                    format!("{host}{}", request.url.path()) == key
                })
                .collect()
        }

        /// The value of a header sent with the Nth request to `key`.
        fn sent_header(&self, key: &str, index: usize, name: &str) -> Option<String> {
            self.requests_to(key).get(index).and_then(|request| {
                request
                    .headers
                    .iter()
                    .find(|(header, _)| header.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone())
            })
        }
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let key = format!(
                "{}{}",
                request.url.host_str().unwrap_or_default(),
                request.url.path()
            );
            let mut pages = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let response = pages.get_mut(&key).map(|queue| {
                // The last scripted response repeats.
                let front = queue
                    .front()
                    .cloned()
                    .unwrap_or_else(|| panic!("a scripted page must exist for {key}"));
                if queue.len() > 1 {
                    queue.pop_front();
                }
                front
            });
            match response {
                Some(scripted) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: scripted.headers,
                    body: scripted.body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// The `search-suggest` fixture handing out a JWT.
    fn suggest() -> Scripted {
        Scripted::json_with_headers(
            &serde_json::json!({"code": 0, "message": "ok", "data": {"items": []}}),
            BTreeMap::from([("x-user".to_string(), r#"{"token":"jwt-token"}"#.to_string())]),
        )
    }

    /// A resolved Dune movie.
    fn dune_media() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: None,
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        }
    }

    /// A Dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::tmdb(438_631, MediaType::Movie)
    }

    /// A provider over the shared mock's TMDB client.
    fn provider(fetcher: &Arc<MockFetcher>) -> MovieBox {
        MovieBox::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
    }

    /// A resolve context over the shared mock.
    fn ctx_for(fetcher: &MockFetcher, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// The play fixture with one direct stream.
    fn play_hakuna() -> serde_json::Value {
        serde_json::json!({
            "data": {"streams": [
                {"url": "https://dl.hakunaymatata.com/v/1080/file.mp4", "resolutions": "1080", "vipLocked": false},
                {"url": "https://dl.hakunaymatata.com/v/720/file.mp4", "resolutions": "720", "vipLocked": true},
                {"url": "https://dl.hakunaymatata.com/v/1080/file.mp4", "resolutions": "1080", "vipLocked": false}
            ]}
        })
    }

    #[tokio::test]
    async fn finds_and_plays_a_movie() -> Result<(), SourceError> {
        let search = serde_json::json!({
            "data": {"items": [
                {"subjectId": "8859944774137176072", "subjectType": 1, "title": "Dune", "releaseDate": "2021-10-22", "detailPath": "dune-xyz"},
                {"subjectId": "9048868765454191080", "subjectType": 1, "title": "Dune", "releaseDate": "1984-12-14", "detailPath": "dune-old"}
            ]}
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    Scripted::json(&search),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/play",
                    Scripted::json(&play_hakuna()),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the search/play fixtures must resolve: {e}"));
        assert_eq!(streams.len(), 1, "vipLocked and duplicate URLs must drop");

        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://dl.hakunaymatata.com/v/1080/file.mp4"
        );
        assert_eq!(stream.format, Format::Mp4);
        assert_eq!(stream.label.as_deref(), Some("Dune (2021) (1080p)"));
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        assert_eq!(stream.meta.source_id.as_deref(), Some("moviebox"));
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://movie-box.co/")
        );
        // The year bonus picked the 2021 subject.
        let play_url = fetcher
            .requests_to("h5-api.aoneroom.com/wefeed-h5api-bff/subject/play")
            .first()
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(
            play_url.contains("subjectId=8859944774137176072"),
            "the year bonus must pick the 2021 remake: {play_url}"
        );
        // The search-suggest JWT travels as the bearer.
        assert_eq!(
            fetcher
                .sent_header(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    0,
                    "Authorization"
                )
                .as_deref(),
            Some("Bearer jwt-token")
        );
        assert_eq!(
            fetcher
                .sent_header(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    0,
                    "X-Request-Lang"
                )
                .as_deref(),
            Some("en")
        );
        Ok(())
    }

    #[tokio::test]
    async fn retries_play_with_the_detail_page_referer() -> Result<(), SourceError> {
        let search = serde_json::json!({
            "data": {"items": [
                {"subjectId": "1", "subjectType": 1, "title": "Dune", "releaseDate": "2021-10-22", "detailPath": "dune-xyz"}
            ]}
        });
        let empty = serde_json::json!({"data": {"streams": []}});
        let play = serde_json::json!({
            "data": {"streams": [
                {"url": "https://cdn.example/dune.mp4", "resolutions": "1080", "vipLocked": false}
            ]}
        });
        let play_key = "h5-api.aoneroom.com/wefeed-h5api-bff/subject/play";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    Scripted::json(&search),
                )
                .serve(play_key, Scripted::json(&empty))
                .serve(play_key, Scripted::json(&play)),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the Referer retry must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), "https://cdn.example/dune.mp4");
        assert!(
            fetcher.sent_header(play_key, 0, "Referer").is_none(),
            "the first play call carries no Referer"
        );
        assert_eq!(
            fetcher.sent_header(play_key, 1, "Referer").as_deref(),
            Some("https://movie-box.co/movies/dune-xyz")
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_variants_fall_through_to_the_punct_stripped_form() -> Result<(), SourceError> {
        let no_hit = serde_json::json!({"data": {"items": []}});
        let hit = serde_json::json!({
            "data": {"items": [
                {"subjectId": "7", "subjectType": 1, "title": "Dune Part Two", "releaseDate": "2024-02-27", "detailPath": "dune-2"}
            ]}
        });
        let play = serde_json::json!({
            "data": {"streams": [
                {"url": "https://cdn.example/dune2.mp4", "resolutions": "2160", "vipLocked": false}
            ]}
        });
        let search_key = "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(search_key, Scripted::json(&no_hit))
                .serve(search_key, Scripted::json(&hit))
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/play",
                    Scripted::json(&play),
                ),
        );
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(ResolvedMedia {
                tmdb_id: Some(693_134),
                imdb_id: None,
                name: "Dune: Part Two".to_string(),
                year: Some(2024),
                season: None,
                episode: None,
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::tmdb(693_134, MediaType::Movie);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the stripped query must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].meta.resolution, Some(2160));
        // The second search POST carried the punctuation-stripped keyword.
        let second = fetcher
            .requests_to(search_key)
            .get(1)
            .and_then(|request| request.body.clone())
            .unwrap_or_default();
        assert!(
            second.contains("Dune Part Two"),
            "stripped keyword in {second}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_maps_season_and_episode() -> Result<(), SourceError> {
        let search = serde_json::json!({
            "data": {"items": [
                {"subjectId": "42", "subjectType": 2, "title": "Breaking Bad", "releaseDate": "2008-01-20", "detailPath": "bb"}
            ]}
        });
        let play = serde_json::json!({
            "data": {"streams": [
                {"url": "https://dl.hakunaymatata.com/v/bb.mp4", "resolutions": "1080", "vipLocked": false}
            ]}
        });
        let play_key = "h5-api.aoneroom.com/wefeed-h5api-bff/subject/play";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    Scripted::json(&search),
                )
                .serve(play_key, Scripted::json(&play)),
        );
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(2),
                episode: Some(3),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 2, 3);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S02E03 (1080p)")
        );
        let play_url = fetcher
            .requests_to(play_key)
            .first()
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(
            play_url.contains("se=2") && play_url.contains("ep=3"),
            "season/episode must map to se/ep: {play_url}"
        );
        // The search carried subjectType 2 for series.
        let search_body = fetcher
            .requests_to("h5-api.aoneroom.com/wefeed-h5api-bff/subject/search")
            .first()
            .and_then(|request| request.body.clone())
            .unwrap_or_default();
        assert!(
            search_body.contains(r#""subjectType":2"#),
            "series subjectType in {search_body}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_search_match_is_not_found() {
        let search = serde_json::json!({
            "data": {"items": [
                {"subjectId": "9", "subjectType": 1, "title": "An unrelated movie", "releaseDate": "1999-01-01", "detailPath": "x"}
            ]}
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    Scripted::json(&search),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("an unmatched search must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn jwt_failure_is_a_miss() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
            Scripted {
                status: 500,
                body: "boom".to_string(),
                headers: BTreeMap::new(),
            },
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a dead JWT mint must be a miss, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_jwtless_success_is_a_structural_surprise() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
            Scripted::json(&serde_json::json!({"code": 0})),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::Scrape { .. }) => {}
            other => panic!("a 200 without the JWT header must be a scrape failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_hakunaymatata_streams_carry_no_referer() -> Result<(), SourceError> {
        let search = serde_json::json!({
            "data": {"items": [
                {"subjectId": "1", "subjectType": 1, "title": "Dune", "releaseDate": "2021-10-22", "detailPath": "dune-xyz"}
            ]}
        });
        let play = serde_json::json!({
            "data": {"streams": [
                {"url": "https://cdn.example/plain.mp4", "resolutions": "720", "vipLocked": false}
            ]}
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search-suggest",
                    suggest(),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/search",
                    Scripted::json(&search),
                )
                .serve(
                    "h5-api.aoneroom.com/wefeed-h5api-bff/subject/play",
                    Scripted::json(&play),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the plain CDN fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert!(!streams[0].meta.request_headers.contains_key("Referer"));
        Ok(())
    }
    #[test]
    fn upgrade_notice_is_not_returned_as_a_movie() {
        let entries = vec![PlayStream {
            url: Some("https://macdn.aoneroom.com/other/upgrade.mp4".into()),
            resolutions: Some("1080".into()),
            vip_locked: Some(false),
        }];
        assert!(build_streams(entries, "Inception").is_empty());
    }
}
