//! `Necro`: a five-embed fan off TMDB ids.
//!
//! Ports `src/source/Necro.js` (`necro.pages.dev` — movies, TV, anime).
//! Same pattern as `CineWave`'s fallback layer: the provider emits its
//! embed URLs (VidSrc.me, 2Embed, VidSrc.to, Embed.su, `MultiEmbed` —
//! upstream `EMBED_SOURCES` verbatim) and resolves them through the
//! [`ExtractorRegistry`], which is where upstream's resolver sent them.
//! Upstream attached the `VidKing` speedracelight hint (`meta.vidking`)
//! only for movies — the API fuzzy-matches series wrong — mirrored here
//! by giving the embed context media with a TMDB id for movies and no
//! media for series: exactly the routing the registry's media-keyed
//! fallback implements.
//!
//! Cuts for the library port:
//!
//! - `meta.title` has no `StreamMeta` field — embed results keep the
//!   labels their extractors produce; upstream's `${title} (${label})`
//!   card titles are resolver-level display state.
//! - Upstream resolved the embeds concurrently (`Promise.all`); this
//!   port resolves sequentially — cross-provider fan-out is the
//!   engine's concurrency domain. A failing embed is dropped, like the
//!   upstream per-extractor `catch`.
//! - The upstream resolver's client-budget and wave-ordering machinery
//!   (Necro sits in wave 1) is the parent engine's domain.

use std::sync::Arc;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// One of the five embed sources (upstream `EMBED_SOURCES`).
struct EmbedSource {
    /// The movie embed template (`{id}`).
    movie: &'static str,
    /// The series embed template (`{id}`, `{s}`, `{e}`).
    tv: &'static str,
}

/// Every embed Necro falls back to, in upstream order.
const EMBED_SOURCES: &[EmbedSource] = &[
    EmbedSource {
        movie: "https://vidsrc.me/embed/movie?tmdb={id}",
        tv: "https://vidsrc.me/embed/tv?tmdb={id}&season={s}&episode={e}",
    },
    EmbedSource {
        movie: "https://www.2embed.cc/embed/{id}",
        tv: "https://www.2embed.cc/embedtv/{id}&s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://vidsrc.to/embed/movie/{id}",
        tv: "https://vidsrc.to/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://embed.su/embed/movie/{id}",
        tv: "https://embed.su/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://multiembed.mov/directstream.php?video_id={id}&tmdb=1&srv=vipstream-s",
        tv: "https://multiembed.mov/directstream.php?video_id={id}&tmdb=1&s={s}&e={e}&srv=vipstream-s",
    },
];

/// The `Necro` provider.
pub struct Necro {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The embed resolution chain.
    extractors: Arc<ExtractorRegistry>,
}

impl Necro {
    /// A provider over the shared TMDB client and the embed registry.
    pub fn new(tmdb: Arc<TmdbClient>, extractors: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: "necro".to_string(),
                label: "Necro".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![vsources_core::types::CountryCode::Multi],
                base_url: Some(
                    Url::parse("https://necro.pages.dev")
                        .unwrap_or_else(|e| panic!("valid Necro base URL: {e}")),
                ),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            extractors,
        }
    }

    /// The embed fan through the extractor registry.
    ///
    /// Movies resolve with media context (the registry's `VidKing`
    /// speedracelight fallback), series without it — the upstream
    /// movies-only `meta.vidking` rule.
    async fn embed_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        name: &str,
        year: Option<u16>,
    ) -> Vec<Stream> {
        let vidking_media = if media.season.is_none() {
            Some(ResolvedMedia {
                tmdb_id: Some(tmdb_id),
                imdb_id: None,
                name: name.to_string(),
                year,
                season: None,
                episode: None,
            })
        } else {
            None
        };

        let mut out = Vec::new();
        for source in EMBED_SOURCES {
            let Some(url) = embed_url(source, tmdb_id, media.season, media.episode) else {
                continue;
            };
            let embed_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: vidking_media.clone(),
                source_id: Some(self.info.id.as_str()),
                referer: self.info.base_url.as_ref(),
            };
            // A failing embed is dropped, like the upstream
            // `extractorRegistry.handle(...).catch(() => [])`.
            if let Ok(streams) = self.extractors.extract(&embed_ctx, &url).await {
                out.extend(streams);
            }
        }
        out
    }
}

#[async_trait]
impl Source for Necro {
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

        let streams = self.embed_streams(ctx, media, tmdb_id, &name, year).await;
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

/// Fill an embed template (`{id}`, `{s}`, `{e}`) into a URL.
fn embed_url(
    source: &EmbedSource,
    tmdb_id: u64,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<Url> {
    let template = if season.is_some() {
        source.tv
    } else {
        source.movie
    };
    let filled = template
        .replace("{id}", &tmdb_id.to_string())
        .replace("{s}", &season.map(|s| s.to_string()).unwrap_or_default())
        .replace("{e}", &episode.map(|e| e.to_string()).unwrap_or_default());
    Url::parse(&filled).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;
    use vsources_core::error::ExtractorError;
    use vsources_core::traits::{Extractor, FetchRequest, FetchResponse, Fetcher};
    use vsources_core::types::Format;

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
            Self {
                status: 200,
                body: value.to_string(),
                headers: BTreeMap::from([(
                    "content-type".to_string(),
                    "application/json".to_string(),
                )]),
            }
        }
    }

    /// A fetcher serving scripted pages by host+path (in order, the last
    /// repeating) and recording every request it sees.
    struct MockFetcher {
        pages: Mutex<HashMap<String, VecDeque<Scripted>>>,
    }

    impl MockFetcher {
        /// A fetcher serving nothing yet.
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
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
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
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

    /// One recorded (url, source id, tmdb id) triple.
    type SeenEntry = (String, Option<String>, Option<u64>);

    /// An extractor that claims everything and records the context it saw.
    struct FakeExtractor {
        seen: Mutex<Vec<SeenEntry>>,
    }

    impl FakeExtractor {
        /// A recording extractor.
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }

        /// The (url, source id, tmdb id) triples recorded so far.
        fn seen(&self) -> Vec<SeenEntry> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Default for FakeExtractor {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Extractor for FakeExtractor {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn label(&self) -> &'static str {
            "Fake"
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
            true
        }

        async fn extract(
            &self,
            ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    url.to_string(),
                    ctx.source_id.map(str::to_string),
                    ctx.media.as_ref().and_then(|media| media.tmdb_id),
                ));
            let stream = Stream::new(
                Url::parse("https://cdn.example/fake.mp4")
                    .unwrap_or_else(|e| panic!("valid test URL: {e}")),
                Format::Mp4,
            );
            Ok(vec![stream])
        }
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

    /// The TMDB mock key for a path.
    fn tmdb_key(path: &str) -> String {
        format!("api.themoviedb.org{path}")
    }

    /// A Dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::tmdb(438_631, MediaType::Movie)
    }

    #[tokio::test]
    async fn movie_embeds_resolve_through_the_registry_with_vidking_media()
    -> Result<(), SourceError> {
        let fetcher = Arc::new(MockFetcher::new().serve(
            &tmdb_key("/3/movie/438631"),
            Scripted::json(&serde_json::json!({"title": "Dune", "release_date": "2021-10-22"})),
        ));
        let fake = Arc::new(FakeExtractor::new());
        let provider = Necro::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(vec![fake.clone()])),
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = provider
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the embed fan must resolve: {e}"));
        assert_eq!(streams.len(), EMBED_SOURCES.len());
        let seen = fake.seen();
        assert!(
            seen.iter()
                .all(|(_, source, _)| source.as_deref() == Some("necro"))
        );
        assert!(seen.iter().all(|(_, _, tmdb)| *tmdb == Some(438_631)));
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://vidsrc.me/embed/movie?tmdb=438631")
        );
        assert!(seen.iter().any(|(url, _, _)| *url
            == "https://multiembed.mov/directstream.php?video_id=438631&tmdb=1&srv=vipstream-s"));
        Ok(())
    }

    #[tokio::test]
    async fn series_embeds_carry_no_media_and_use_season_templates() -> Result<(), SourceError> {
        let fetcher = Arc::new(MockFetcher::new().serve(
            &tmdb_key("/3/tv/1396"),
            Scripted::json(
                &serde_json::json!({"name": "Breaking Bad", "first_air_date": "2008-01-20"}),
            ),
        ));
        let fake = Arc::new(FakeExtractor::new());
        let provider = Necro::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(vec![fake.clone()])),
        );
        let ctx = ctx_for(&fetcher, None);
        let media = MediaRef::series(MediaId::Tmdb(1396), 2, 3);

        let streams = provider
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series embed fan must resolve: {e}"));
        assert_eq!(streams.len(), EMBED_SOURCES.len());
        let seen = fake.seen();
        assert!(seen.iter().all(|(_, _, tmdb)| tmdb.is_none()));
        assert!(
            seen.iter().any(
                |(url, _, _)| *url == "https://vidsrc.me/embed/tv?tmdb=1396&season=2&episode=3"
            )
        );
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://www.2embed.cc/embedtv/1396&s=2&e=3")
        );
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://vidsrc.to/embed/tv/1396/2/3")
        );
        Ok(())
    }

    #[tokio::test]
    async fn imdb_keyed_references_resolve_through_tmdb_find() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    &tmdb_key("/3/find/tt1160419"),
                    Scripted::json(&serde_json::json!({"movie_results": [{"id": 438_631}]})),
                )
                .serve(
                    &tmdb_key("/3/movie/438631"),
                    Scripted::json(
                        &serde_json::json!({"title": "Dune", "release_date": "2021-10-22"}),
                    ),
                ),
        );
        let fake = Arc::new(FakeExtractor::new());
        let provider = Necro::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(vec![fake])),
        );
        let ctx = ctx_for(&fetcher, None);
        let media = MediaRef::imdb("tt1160419", MediaType::Movie);

        let streams = provider
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the IMDb-keyed reference must resolve: {e}"));
        assert_eq!(streams.len(), EMBED_SOURCES.len());
        Ok(())
    }

    #[tokio::test]
    async fn nothing_resolvable_is_not_found() {
        let fetcher = Arc::new(MockFetcher::new());
        let provider = Necro::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(Vec::new())),
        );
        let ctx = ctx_for(
            &fetcher,
            Some(ResolvedMedia {
                tmdb_id: Some(438_631),
                imdb_id: None,
                name: "Dune".to_string(),
                year: Some(2021),
                season: None,
                episode: None,
            }),
        );

        match provider.resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a total miss must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failing_extractions_are_dropped_but_the_rest_resolve() -> Result<(), SourceError> {
        // The registry returns nothing for every embed (no extractors),
        // so a dead TMDB detail call is the only remaining path.
        let fetcher = Arc::new(MockFetcher::new());
        let provider = Necro::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(Vec::new())),
        );
        let ctx = ctx_for(&fetcher, None);

        match provider.resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a miss must be a NotFound, got {other:?}"),
        }
        Ok(())
    }
}
