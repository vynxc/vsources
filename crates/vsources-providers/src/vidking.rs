//! `VidKing`: TMDB-keyed embeds at `vidking.net`.
//!
//! Ports `src/source/VidKing.js`:
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`), then fetch name/year and the
//!    `IMDb` id (`getTmdbNameAndYear`, `getImdbId` — the latter best-effort).
//! 2. Emit one embed URL — `/embed/movie/{id}` or `/embed/tv/{id}/{s}/{e}`
//!    — and resolve it through the [`ExtractorRegistry`]. The embed page
//!    is a React SPA; the dedicated `VidKing` extractor (the
//!    `api.speedracelight.com` backend, TMDB-based) is the registry's
//!    media fallback: the extract context carries media with a TMDB id
//!    **for movies only** (`vidkingMeta = tmdbId.season ? null : {…}`) —
//!    the API returns wrong content for series. Forwarding the title,
//!    year, and `IMDb` id here saves the extractor a duplicate TMDB
//!    round-trip (the JS's reason for passing `meta.vidking`).
//!
//! Cuts from the upstream, mapped rather than dropped:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{name} ({year})` / `{name} S01E02`) is carried as [`Stream::label`]
//!   on every resolved stream.
//! - `meta.vidking` — the registry joins the `vidking` extractor when the
//!   extract context carries media with a TMDB id (movies only here).
//! - upstream sources returned embed URLs that `StreamResolver` extracted
//!   afterwards; this provider resolves inline through the registry, and
//!   one embed's failure is skipped (the resolver's `.catch(() => [])`).

use std::sync::Arc;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id, upstream `this.id`.
const ID: &str = "vidking";
/// The display label, upstream `this.label`.
const LABEL: &str = "VidKing";
/// The embed origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://www.vidking.net";

/// The `VidKing` provider.
pub struct VidKing {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The registry the embed resolves through.
    extractors: Arc<ExtractorRegistry>,
    /// TMDB identity and metadata resolution.
    tmdb: Arc<TmdbClient>,
}

impl VidKing {
    /// Build the provider over an extractor registry and a TMDB client.
    #[must_use]
    pub fn new(extractors: Arc<ExtractorRegistry>, tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            extractors,
            tmdb,
        }
    }
}

#[async_trait]
impl Source for VidKing {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = resolve_tmdb_id(ctx, media, &self.tmdb).await?;
        let (name, year) = name_and_year(ctx, media, &self.tmdb, tmdb_id).await?;
        // Best-effort — upstream wraps the lookup in try/catch; it runs
        // for series too, even though only movies forward it.
        let imdb_id = best_effort_imdb(ctx, media, &self.tmdb, tmdb_id).await;
        let is_tv = media.season.is_some();
        let title = embed_title(&name, year, media);

        let embed = if is_tv {
            embed_url(&format!(
                "{BASE_URL}/embed/tv/{}/{}/{}",
                tmdb_id,
                media.season.unwrap_or(1),
                media.episode.unwrap_or(1)
            ))
        } else {
            embed_url(&format!("{BASE_URL}/embed/movie/{tmdb_id}"))
        }?;

        // Movies only — `const vidkingMeta = tmdbId.season ? null : {…}`.
        let extract_media = (!is_tv).then(|| ResolvedMedia {
            tmdb_id: Some(tmdb_id),
            imdb_id,
            name: name.clone(),
            year,
            season: None,
            episode: None,
        });

        let sub_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: extract_media,
            source_id: Some(ID),
            referer: None,
        };

        let streams = self
            .extractors
            .extract(&sub_ctx, &embed)
            .await
            .unwrap_or_default();
        Ok(streams
            .into_iter()
            .map(|stream| tagged(stream, &title))
            .collect())
    }
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn resolve_tmdb_id(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => tmdb.tmdb_id_from_imdb(imdb, media.kind).await,
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear` (whose errors propagate upstream).
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = tmdb.name_and_year(tmdb_id, media.kind, None).await?;
    Ok((name.name, name.year))
}

/// `getImdbId` — the reference's own id when IMDb-keyed, the pre-resolved
/// media or `/external_ids` otherwise; failures are best-effort `None`
/// (upstream: `try { … } catch { }`).
async fn best_effort_imdb(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Option<String> {
    match &media.id {
        MediaId::Imdb(imdb) => Some(imdb.clone()),
        MediaId::Tmdb(_) => {
            if let Some(pre_resolved) = ctx.media.as_ref().and_then(|media| media.imdb_id.clone()) {
                return Some(pre_resolved);
            }
            tmdb.imdb_id_from_tmdb(tmdb_id, media.kind)
                .await
                .ok()
                .flatten()
        }
    }
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the upstream
/// `meta.title`, carried as the stream label.
fn embed_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// A rendered embed URL; a parse failure is a structural surprise.
fn embed_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw)
        .map_err(|error| SourceError::scrape(ID, format!("invalid embed URL `{raw}`: {error}")))
}

/// Attach the provider identity and the JS's title to an
/// extractor-produced stream.
fn tagged(mut stream: Stream, title: &str) -> Stream {
    stream.label = Some(title.to_string());
    stream.meta.languages = vec![CountryCode::Multi];
    stream.meta.source_id = Some(ID.to_string());
    stream.meta.source_label = Some(LABEL.to_string());
    stream
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use url::Url;
    use vsources_core::error::{ExtractorError, FetchError, SourceError};
    use vsources_core::tmdb::TmdbClient;
    use vsources_core::traits::{
        Extractor, FetchRequest, FetchResponse, Fetcher, ResolveCtx, ResolvedMedia, Source,
    };
    use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, Stream};
    use vsources_extractors::ExtractorRegistry;

    use super::VidKing;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or `path?query`)
    /// in call order — the last body repeats — recording every request.
    /// Query-bearing lookups fall back to the bare path, so TMDB requests
    /// (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<String>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `body`; earlier registrations pop first.
        fn page(self, key: impl Into<String>, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push(body.into());
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    /// The lookup key of a URL: `path?query` when a query is present.
    fn key_of(url: &Url) -> String {
        match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = key_of(&request.url);
            let body = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let body = match body {
                Some(body) => Some(body),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(request.url.path())
                    .map(|bodies| bodies[0].clone()),
            };
            match body {
                Some(body) => Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "text/html".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    // -- the stub extractor --------------------------------------------------

    /// One extraction a stub observed.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        url: String,
        source_id: Option<String>,
        tmdb_id: Option<u64>,
        imdb_id: Option<String>,
        name: Option<String>,
        season: Option<u32>,
    }

    /// What a stub answers.
    enum Outcome {
        /// `ExtractorError::NotFound`.
        Miss,
        /// One direct HLS stream.
        Direct,
    }

    /// An extractor that records its calls and answers a canned result —
    /// with the id `vidking` it doubles as the registry's media fallback.
    struct StubExtractor {
        id: &'static str,
        label: &'static str,
        hosts: &'static [&'static str],
        outcome: Outcome,
        calls: Mutex<Vec<Call>>,
    }

    impl StubExtractor {
        /// A stub claiming `hosts` with the given outcome.
        fn build(id: &'static str, hosts: &'static [&'static str], outcome: Outcome) -> Arc<Self> {
            Arc::new(Self {
                id,
                label: id,
                hosts,
                outcome,
                calls: Mutex::new(Vec::new()),
            })
        }

        /// The recorded calls.
        fn calls(&self) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Extractor for StubExtractor {
        fn id(&self) -> &str {
            self.id
        }

        fn label(&self) -> &str {
            self.label
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            self.hosts
                .iter()
                .any(|host| url.host_str().is_some_and(|h| h.ends_with(host)))
        }

        async fn extract(
            &self,
            ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Call {
                    url: url.to_string(),
                    source_id: ctx.source_id.map(str::to_string),
                    tmdb_id: ctx.media.as_ref().and_then(|media| media.tmdb_id),
                    imdb_id: ctx.media.as_ref().and_then(|media| media.imdb_id.clone()),
                    name: ctx.media.as_ref().map(|media| media.name.clone()),
                    season: ctx.media.as_ref().and_then(|media| media.season),
                });
            match self.outcome {
                Outcome::Miss => Err(ExtractorError::NotFound),
                Outcome::Direct => Ok(vec![Stream::new(
                    Url::parse("https://cdn.example.com/hls/master.m3u8")
                        .unwrap_or_else(|error| panic!("valid test URL: {error}")),
                    Format::Hls,
                )]),
            }
        }
    }

    // -- fixtures ------------------------------------------------------------

    /// The provider over a registry of `stubs` and a TMDB client that
    /// shares the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>, stubs: &[Arc<StubExtractor>]) -> VidKing {
        let extractors = ExtractorRegistry::new(
            stubs
                .iter()
                .map(|stub| {
                    let extractor: Arc<dyn Extractor> = stub.clone();
                    extractor
                })
                .collect(),
        );
        let tmdb = TmdbClient::new("test-key", mock.clone());
        VidKing::new(Arc::new(extractors), Arc::new(tmdb))
    }

    /// A context over the scripted fetcher, optionally with media.
    fn ctx_for(fetcher: &Arc<ScriptedFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = fetcher.as_ref();
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, &[]);
        let info = provider.info();
        assert_eq!(info.id, "vidking");
        assert_eq!(info.label, "VidKing");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://www.vidking.net/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_the_movie_embed_with_imdb_hints() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    "/3/movie/550",
                    r#"{"title":"Fight Club","release_date":"1999-10-15"}"#,
                )
                .page("/3/movie/550/external_ids", r#"{"imdb_id":"tt0137523"}"#),
        );
        let generic = StubExtractor::build("generic", &["vidking.net"], Outcome::Miss);
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic.clone(), vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(550)))
            .await?;

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://cdn.example.com/hls/master.m3u8"
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.label.as_deref(), Some("Fight Club (1999)"));
        assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        assert_eq!(stream.meta.source_id.as_deref(), Some("vidking"));
        assert_eq!(stream.meta.source_label.as_deref(), Some("VidKing"));

        assert_eq!(generic.calls().len(), 1);
        assert_eq!(
            generic.calls()[0].url,
            "https://www.vidking.net/embed/movie/550"
        );
        // The media fallback carried the id hints the speedracelight API
        // needs — no duplicate TMDB round-trip in the extractor.
        let calls = vidking.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tmdb_id, Some(550));
        assert_eq!(calls[0].imdb_id.as_deref(), Some("tt0137523"));
        assert_eq!(calls[0].name.as_deref(), Some("Fight Club"));
        assert_eq!(calls[0].season, None);
        Ok(())
    }

    #[tokio::test]
    async fn series_embeds_skip_the_media_fallback() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    "/3/tv/1396",
                    r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
                )
                .page("/3/tv/1396/external_ids", r#"{"imdb_id":"tt0903747"}"#),
        );
        let generic = StubExtractor::build("generic", &["vidking.net"], Outcome::Direct);
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic.clone(), vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("Breaking Bad S01E02"));
        assert_eq!(
            generic.calls()[0].url,
            "https://www.vidking.net/embed/tv/1396/1/2"
        );
        assert!(
            vidking.calls().is_empty(),
            "series must not reach the vidking fallback"
        );
        Ok(())
    }

    #[tokio::test]
    async fn imdb_references_pass_their_own_id() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/3/find/tt0137523", r#"{"movie_results":[{"id":550}]}"#)
                .page(
                    "/3/movie/550",
                    r#"{"title":"Fight Club","release_date":"1999-10-15"}"#,
                ),
        );
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, std::slice::from_ref(&vidking));
        let ctx = ctx_for(&mock, None);

        // getImdbId is a pass-through for IMDb-keyed references — no
        // `/external_ids` call for this media.
        provider
            .resolve(
                &ctx,
                &MediaRef::movie(MediaId::Imdb("tt0137523".to_string())),
            )
            .await?;

        let calls = vidking.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].imdb_id.as_deref(), Some("tt0137523"));
        let external_ids = mock
            .requests()
            .iter()
            .any(|request| request.url.path().ends_with("external_ids"));
        assert!(!external_ids);
        Ok(())
    }

    #[tokio::test]
    async fn tmdb_miss_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default().page("/3/find/tt0000000", r#"{"movie_results":[]}"#),
        );
        let provider = provider(&mock, &[]);
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(
                &ctx,
                &MediaRef::movie(MediaId::Imdb("tt0000000".to_string())),
            )
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("an unmapped IMDb id must be a NotFound, got {other:?}"),
        }
    }
}
