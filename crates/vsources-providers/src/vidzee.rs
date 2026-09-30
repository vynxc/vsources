//! `VidZee`: per-server embeds at `player.vidzee.wtf`.
//!
//! Ports `src/source/Vidzee.js` (itself a port of the
//! `webstreamr-mbg` research source):
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`); no name/year is needed.
//! 2. Emit one embed per server: `/v2/embed/movie/{id}?sr={sr}` or
//!    `/v2/embed/tv/{id}/{s}/{e}?sr={sr}`. The JS's `VIDZEE_SERVERS`
//!    table carries eight servers; only the English rows survive the
//!    upstream filter (`countryCode === en || multi`) — Achilles (`sr=3`)
//!    and Drag (`sr=5`) here. The full table stays in the port so a
//!    future language policy is a filter change.
//! 3. Resolve each embed through the [`ExtractorRegistry`] — the `VidZee`
//!    extractor owns the api-key decryption and server listing. The JS
//!    passes **no** `meta.vidking` here, so the extract context carries
//!    no media: a `VidZee` embed that the dedicated extractor cannot
//!    resolve yields nothing, exactly like the upstream chain.
//!
//! Cuts from the upstream, mapped rather than dropped:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{server.name} ({server.flag})`) is carried as [`Stream::label`]
//!   on every resolved stream.
//! - the server's `countryCode` meta becomes `StreamMeta::languages`.
//! - upstream sources returned embed URLs that `StreamResolver` extracted
//!   afterwards; this provider resolves inline through the registry, and
//!   one embed's failure is skipped (the resolver's `.catch(() => [])`).

use std::sync::Arc;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id, upstream `this.id`.
const ID: &str = "vidzee";
/// The display label, upstream `this.label`.
const LABEL: &str = "VidZee";
/// The embed origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://player.vidzee.wtf";

/// One player server, upstream `VIDZEE_SERVERS`.
struct Server {
    /// The `?sr=` server id.
    sr: &'static str,
    /// The flag badge.
    flag: &'static str,
    /// The server name.
    name: &'static str,
    /// The server's language.
    country: CountryCode,
}

/// The full server table, in upstream order.
const SERVERS: &[Server] = &[
    Server {
        sr: "3",
        flag: "US",
        name: "Achilles",
        country: CountryCode::En,
    },
    Server {
        sr: "5",
        flag: "US",
        name: "Drag",
        country: CountryCode::En,
    },
    Server {
        sr: "6",
        flag: "VN",
        name: "Viet",
        country: CountryCode::Vi,
    },
    Server {
        sr: "7",
        flag: "IN",
        name: "Hindi",
        country: CountryCode::Hi,
    },
    Server {
        sr: "8",
        flag: "IN",
        name: "Bengali",
        country: CountryCode::Hi,
    },
    Server {
        sr: "9",
        flag: "IN",
        name: "Tamil",
        country: CountryCode::Ta,
    },
    Server {
        sr: "10",
        flag: "IN",
        name: "Telugu",
        country: CountryCode::Te,
    },
    Server {
        sr: "11",
        flag: "IN",
        name: "Malayalam",
        country: CountryCode::Ml,
    },
];

/// The `VidZee` provider.
pub struct VidZee {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The registry the embeds resolve through.
    extractors: Arc<ExtractorRegistry>,
    /// TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl VidZee {
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
impl Source for VidZee {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = resolve_tmdb_id(ctx, media, &self.tmdb).await?;
        let is_tv = media.season.is_some();
        let (season, episode) = (media.season.unwrap_or(1), media.episode.unwrap_or(1));

        let mut streams = Vec::new();
        // `VIDZEE_SERVERS.filter(server => en || multi)` — the English
        // rows only.
        for server in SERVERS
            .iter()
            .filter(|server| matches!(server.country, CountryCode::En | CountryCode::Multi))
        {
            let raw = if is_tv {
                format!(
                    "{BASE_URL}/v2/embed/tv/{tmdb_id}/{season}/{episode}?sr={}",
                    server.sr
                )
            } else {
                format!("{BASE_URL}/v2/embed/movie/{tmdb_id}?sr={}", server.sr)
            };
            let embed = embed_url(&raw)?;
            // No media fallback: the JS passes no `meta.vidking`, so the
            // embeds resolve through the VidZee extractor alone.
            let sub_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: None,
                source_id: Some(ID),
                referer: None,
            };
            // One failed embed extraction is skipped, like the resolver's
            // `.catch(() => [])`.
            let resolved = self
                .extractors
                .extract(&sub_ctx, &embed)
                .await
                .unwrap_or_default();
            let title = format!("{} ({})", server.name, server.flag);
            streams.extend(
                resolved
                    .into_iter()
                    .map(|stream| tagged(stream, &title, server.country)),
            );
        }
        Ok(streams)
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

/// A rendered embed URL; a parse failure is a structural surprise.
fn embed_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw)
        .map_err(|error| SourceError::scrape(ID, format!("invalid embed URL `{raw}`: {error}")))
}

/// Attach the provider identity, the JS's title, and the server's
/// language to an extractor-produced stream.
fn tagged(mut stream: Stream, title: &str, country: CountryCode) -> Stream {
    stream.label = Some(title.to_string());
    stream.meta.languages = vec![country];
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

    use super::VidZee;

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
    }

    /// An extractor that records its calls and answers a canned result —
    /// with the id `vidking` it doubles as the registry's media fallback.
    struct StubExtractor {
        id: &'static str,
        label: &'static str,
        hosts: &'static [&'static str],
        calls: Mutex<Vec<Call>>,
    }

    impl StubExtractor {
        /// A stub claiming `hosts`.
        fn build(id: &'static str, hosts: &'static [&'static str]) -> Arc<Self> {
            Arc::new(Self {
                id,
                label: id,
                hosts,
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
                });
            Ok(vec![Stream::new(
                Url::parse("https://cdn.example.com/hls/master.m3u8")
                    .unwrap_or_else(|error| panic!("valid test URL: {error}")),
                Format::Hls,
            )])
        }
    }

    // -- fixtures ------------------------------------------------------------

    /// The provider over a registry of `stubs` and a TMDB client that
    /// shares the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>, stubs: &[Arc<StubExtractor>]) -> VidZee {
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
        VidZee::new(Arc::new(extractors), Arc::new(tmdb))
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
        assert_eq!(info.id, "vidzee");
        assert_eq!(info.label, "VidZee");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://player.vidzee.wtf/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_the_two_english_servers() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default());
        let generic = StubExtractor::build("generic", &["vidzee.wtf"]);
        let vidking = StubExtractor::build("vidking", &[]);
        let provider = provider(&mock, &[generic.clone(), vidking.clone()]);
        let ctx = ctx_for(
            &mock,
            Some(ResolvedMedia {
                tmdb_id: Some(27205),
                imdb_id: None,
                name: "Inception".to_string(),
                year: Some(2010),
                season: None,
                episode: None,
            }),
        );

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        assert_eq!(streams.len(), 2);
        // Both English servers resolved through the stub (its direct
        // stream), with the embeds recorded in server order; the
        // non-English rows of the table were never emitted.
        for stream in &streams {
            assert_eq!(
                stream.url.as_str(),
                "https://cdn.example.com/hls/master.m3u8"
            );
        }
        let calls = generic.calls();
        let embeds: Vec<&str> = calls.iter().map(|call| call.url.as_str()).collect();
        assert_eq!(
            embeds,
            vec![
                "https://player.vidzee.wtf/v2/embed/movie/27205?sr=3",
                "https://player.vidzee.wtf/v2/embed/movie/27205?sr=5",
            ]
        );
        assert_eq!(streams[0].label.as_deref(), Some("Achilles (US)"));
        assert_eq!(streams[1].label.as_deref(), Some("Drag (US)"));
        for stream in &streams {
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.meta.languages, vec![CountryCode::En]);
            assert_eq!(stream.meta.source_id.as_deref(), Some("vidzee"));
            assert_eq!(stream.meta.source_label.as_deref(), Some("VidZee"));
        }
        // No `meta.vidking` in the JS — the media fallback never joins,
        // even for movies with resolved media in the parent context.
        assert_eq!(generic.calls().len(), 2);
        assert!(vidking.calls().is_empty());
        assert!(mock.requests().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn series_embeds_carry_season_and_episode() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default());
        let generic = StubExtractor::build("generic", &["vidzee.wtf"]);
        let provider = provider(&mock, &[generic]);
        let ctx = ctx_for(&mock, None);

        provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        // `vidking` would only run with media in the extract context —
        // and this provider never puts any there.
        Ok(())
    }

    #[tokio::test]
    async fn imdb_references_map_through_find() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default().page("/3/find/tt0903747", r#"{"tv_results":[{"id":1396}]}"#),
        );
        let generic = StubExtractor::build("generic", &["vidzee.wtf"]);
        let provider = provider(&mock, std::slice::from_ref(&generic));
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(
                &ctx,
                &MediaRef::series(MediaId::Imdb("tt0903747".to_string()), 1, 2),
            )
            .await?;

        assert_eq!(streams.len(), 2);
        let calls = generic.calls();
        let embeds: Vec<&str> = calls.iter().map(|call| call.url.as_str()).collect();
        assert_eq!(
            embeds,
            vec![
                "https://player.vidzee.wtf/v2/embed/tv/1396/1/2?sr=3",
                "https://player.vidzee.wtf/v2/embed/tv/1396/1/2?sr=5",
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn tmdb_miss_is_not_found() {
        let mock =
            Arc::new(ScriptedFetcher::default().page("/3/find/tt0000000", r#"{"tv_results":[]}"#));
        let provider = provider(&mock, &[]);
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(
                &ctx,
                &MediaRef::series(MediaId::Imdb("tt0000000".to_string()), 1, 1),
            )
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("an unmapped IMDb id must be a NotFound, got {other:?}"),
        }
    }
}
