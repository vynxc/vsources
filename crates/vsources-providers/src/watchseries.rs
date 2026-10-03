//! `WatchSeries`: eleven TMDB-keyed embed servers.
//! Its explicit `VidLink` server uses the native API first, preserving headers
//! and exact TV identity; legacy embed routing remains a fallback.
//!
//! Ports `src/source/WatchSeries.js`:
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`), then fetch name/year
//!    (`getTmdbNameAndYear`).
//! 2. Emit the eleven static embed URLs (the `SERVERS` map from the
//!    site's JS bundle analysis — upstream's `?server=` keys are kept as
//!    comments per row). Movies and series use per-server templates,
//!    including the query-string variants (`autoplay`, `tmdb=1`,
//!    `season`/`episode`).
//! 3. Resolve each embed through the [`ExtractorRegistry`]. The JS passed
//!    `meta.vidking` for **movies only** — the speedracelight API
//!    resolves embeds that have no dedicated extractor (vidlink.pro,
//!    2embed.cc, vidfast.pro, …); without it those produce zero streams.
//!    For series the API returns wrong content, so it is skipped. The
//!    registry's media fallback implements that routing: the extract
//!    context carries media with a TMDB id for movies only.
//!
//! Cuts from the upstream, mapped rather than dropped:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{name} ({year})` / `{name} S01E02`, plus the server label) is
//!   carried as [`Stream::label`] on every resolved stream.
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
const ID: &str = "watchseries";
/// The display label, upstream `this.label`.
const LABEL: &str = "WatchSeries";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://watchseries.lc";

/// One server's embed templates, upstream `SERVERS`.
struct Server {
    /// Card label.
    label: &'static str,
    /// Movie embed template.
    movie: &'static str,
    /// TV embed template.
    tv: &'static str,
}

/// The eleven servers, in upstream order (keys: vidsrcto, vidsrcfyi,
/// vidrock, vidnest, vidking, vidlink, vidfast, 2embed, multiembed,
/// superflix, peachify).
const SERVERS: &[Server] = &[
    Server {
        label: "VidSrc",
        movie: "https://vidsrc.mov/embed/movie/{id}",
        tv: "https://vidsrc.mov/embed/tv/{id}/{s}/{e}",
    },
    Server {
        label: "VidSrc.fyi",
        movie: "https://vidsrc.fyi/embed/movie/{id}",
        tv: "https://vidsrc.fyi/embed/tv/{id}/{s}/{e}",
    },
    Server {
        label: "VidRock",
        movie: "https://vidrock.net/movie/{id}",
        tv: "https://vidrock.net/tv/{id}/{s}/{e}",
    },
    Server {
        label: "Vidnest",
        movie: "https://vidnest.fun/movie/{id}",
        tv: "https://vidnest.fun/tv/{id}/{s}/{e}",
    },
    Server {
        label: "VidKing",
        movie: "https://www.vidking.net/embed/movie/{id}",
        tv: "https://www.vidking.net/embed/tv/{id}/{s}/{e}",
    },
    Server {
        label: "VidLink",
        movie: "https://vidlink.pro/movie/{id}?autoplay=true&title=true",
        tv: "https://vidlink.pro/tv/{id}/{s}/{e}?autoplay=true&title=true",
    },
    Server {
        label: "VidFast",
        movie: "https://vidfast.pro/movie/{id}?autoPlay=true",
        tv: "https://vidfast.pro/tv/{id}/{s}/{e}?autoPlay=true",
    },
    Server {
        label: "2Embed",
        movie: "https://www.2embed.cc/embed/{id}",
        // The upstream template's `&` sits in the path, verbatim.
        tv: "https://www.2embed.cc/embedtv/{id}&s={s}&e={e}",
    },
    Server {
        label: "MultiEmbed",
        movie: "https://multiembed.mov/?video_id={id}&tmdb=1",
        tv: "https://multiembed.mov/?video_id={id}&tmdb=1&s={s}&e={e}",
    },
    Server {
        label: "SuperFlix",
        movie: "https://superflixapi.co/filme/{id}",
        tv: "https://superflixapi.co/serie/{id}/{s}/{e}",
    },
    Server {
        label: "Peachify",
        movie: "https://peachify.top/embed/movie/{id}",
        tv: "https://peachify.top/embed/tv/{id}?season={s}&episode={e}",
    },
];

/// The `WatchSeries` provider.
pub struct WatchSeries {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The registry the embeds resolve through.
    extractors: Arc<ExtractorRegistry>,
    /// TMDB identity and metadata resolution.
    tmdb: Arc<TmdbClient>,
    /// Native resolver for the `VidLink` server this site already offers.
    vidlink: crate::vidlink::VidLink,
}

impl WatchSeries {
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
            vidlink: crate::vidlink::VidLink::new(Arc::clone(&tmdb)),
            tmdb,
        }
    }
}

#[async_trait]
impl Source for WatchSeries {
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
        let is_tv = media.season.is_some();
        let title = embed_title(&name, year, media);
        let (season, episode) = (media.season.unwrap_or(1), media.episode.unwrap_or(1));

        // VidLink is an explicit site server, but generic extraction previously
        // routed it into the unavailable speedracelight fallback. Resolve its
        // actual API first and keep exact TV season/episode context.
        let native_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: Some(ResolvedMedia {
                tmdb_id: Some(tmdb_id),
                imdb_id: None,
                name: name.clone(),
                year,
                season: media.season,
                episode: media.episode,
            }),
            source_id: Some(ID),
            referer: None,
        };
        if let Ok(native) = self.vidlink.resolve(&native_ctx, media).await
            && !native.is_empty()
        {
            let label = format!("{title} (VidLink)");
            return Ok(native
                .into_iter()
                .map(|stream| tagged(stream, &label))
                .collect());
        }

        // `const vidkingMeta = tmdbId.season ? null : {…}` — movies only.
        let extract_media = (!is_tv).then(|| ResolvedMedia {
            tmdb_id: Some(tmdb_id),
            imdb_id: None,
            name: name.clone(),
            year,
            season: None,
            episode: None,
        });

        let mut streams = Vec::new();
        for server in SERVERS {
            let raw = if is_tv {
                render(server.tv, tmdb_id, season, episode)
            } else {
                render(server.movie, tmdb_id, season, episode)
            };
            let embed = embed_url(&raw)?;
            let sub_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: extract_media.clone(),
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
            let label = format!("{title} ({})", server.label);
            streams.extend(resolved.into_iter().map(|stream| tagged(stream, &label)));
        }
        Ok(streams)
    }
}

/// Render a template's `{id}`/`{s}`/`{e}` slots — the JS `replace` chain.
fn render(template: &str, tmdb_id: u64, season: u32, episode: u32) -> String {
    template
        .replace("{id}", &tmdb_id.to_string())
        .replace("{s}", &season.to_string())
        .replace("{e}", &episode.to_string())
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
fn tagged(mut stream: Stream, label: &str) -> Stream {
    stream.label = Some(label.to_string());
    if stream.meta.languages.is_empty() {
        stream.meta.languages = vec![CountryCode::Multi];
    }
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

    use super::WatchSeries;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or `path?query`)
    /// in call order — the last body repeats — recording every request.
    /// Query-bearing lookups fall back to the bare path, so TMDB requests
    /// (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<String>>>,
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
        name: Option<String>,
        season: Option<u32>,
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
                    name: ctx.media.as_ref().map(|media| media.name.clone()),
                    season: ctx.media.as_ref().and_then(|media| media.season),
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
    fn provider(mock: &Arc<ScriptedFetcher>, stubs: &[Arc<StubExtractor>]) -> WatchSeries {
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
        WatchSeries::new(Arc::new(extractors), Arc::new(tmdb))
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
        assert_eq!(info.id, "watchseries");
        assert_eq!(info.label, "WatchSeries");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://watchseries.lc/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn emits_all_eleven_movie_embeds() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/movie/27205",
            r#"{"title":"Inception","release_date":"2010-07-16"}"#,
        ));
        let vidking = StubExtractor::build("vidking", &[]);
        let provider = provider(&mock, std::slice::from_ref(&vidking));
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        // All eleven servers resolve through the media fallback.
        assert_eq!(streams.len(), 11);
        let calls = vidking.calls();
        assert_eq!(calls.len(), 11);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://vidsrc.mov/embed/movie/27205",
                "https://vidsrc.fyi/embed/movie/27205",
                "https://vidrock.net/movie/27205",
                "https://vidnest.fun/movie/27205",
                "https://www.vidking.net/embed/movie/27205",
                "https://vidlink.pro/movie/27205?autoplay=true&title=true",
                "https://vidfast.pro/movie/27205?autoPlay=true",
                "https://www.2embed.cc/embed/27205",
                "https://multiembed.mov/?video_id=27205&tmdb=1",
                "https://superflixapi.co/filme/27205",
                "https://peachify.top/embed/movie/27205",
            ]
        );
        for call in &calls {
            assert_eq!(call.tmdb_id, Some(27205));
            assert_eq!(call.name.as_deref(), Some("Inception"));
            assert_eq!(call.source_id.as_deref(), Some("watchseries"));
        }
        // Labels carry the JS's title + server segment.
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Inception (2010) (VidSrc)")
        );
        assert_eq!(
            streams[5].label.as_deref(),
            Some("Inception (2010) (VidLink)")
        );
        assert_eq!(
            streams[10].label.as_deref(),
            Some("Inception (2010) (Peachify)")
        );
        for stream in &streams {
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
            assert_eq!(stream.meta.source_id.as_deref(), Some("watchseries"));
            assert_eq!(stream.meta.source_label.as_deref(), Some("WatchSeries"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn series_embeds_skip_the_media_fallback() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/tv/1396",
            r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
        ));
        let generic = StubExtractor::build(
            "generic",
            &[
                "vidsrc.mov",
                "vidsrc.fyi",
                "vidrock.net",
                "vidnest.fun",
                "vidking.net",
                "vidlink.pro",
                "vidfast.pro",
                "2embed.cc",
                "multiembed.mov",
                "superflixapi.co",
                "peachify.top",
            ],
        );
        let vidking = StubExtractor::build("vidking", &[]);
        let provider = provider(&mock, &[generic.clone(), vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        assert_eq!(streams.len(), 11);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 (VidSrc)")
        );
        // The per-server series templates, including the odd 2embed path
        // and the query-bearing forms — the resolved streams are the
        // stub's direct stream; the embeds are what it recorded.
        let calls = generic.calls();
        let embeds: Vec<&str> = calls.iter().map(|call| call.url.as_str()).collect();
        assert_eq!(embeds[0], "https://vidsrc.mov/embed/tv/1396/1/2");
        assert_eq!(embeds[7], "https://www.2embed.cc/embedtv/1396&s=1&e=2");
        assert_eq!(
            embeds[8],
            "https://multiembed.mov/?video_id=1396&tmdb=1&s=1&e=2"
        );
        assert_eq!(
            embeds[10],
            "https://peachify.top/embed/tv/1396?season=1&episode=2"
        );
        // `vidkingMeta` is null for series — the fallback never joins.
        assert!(
            vidking.calls().is_empty(),
            "series must not reach the vidking fallback"
        );
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
    #[tokio::test]
    async fn native_vidlink_route_preserves_tv_identity_and_provider_tag() -> Result<(), SourceError>
    {
        let mock = Arc::new(ScriptedFetcher::default()
            .page("/3/tv/1396", r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#)
            .page("/api/enc-vidlink?text=1396", r#"{"result":"test-cipher"}"#)
            .page("/api/b/tv/test-cipher/2/1", r#"{"stream":{"qualities":{"720":{"url":"https://cdn.example/episode-s2e1.mp4"}}}}"#));
        let streams = provider(&mock, &[])
            .resolve(
                &ctx_for(&mock, None),
                &MediaRef::series(MediaId::Tmdb(1396), 2, 1),
            )
            .await?;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.path(), "/episode-s2e1.mp4");
        assert_eq!(streams[0].meta.source_id.as_deref(), Some("watchseries"));
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("S02E01") && label.contains("VidLink"))
        );
        Ok(())
    }
}
