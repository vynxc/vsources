//! `VidSrcSbs`: three TMDB-keyed embed servers.
//!
//! Ports `src/source/VidSrcSbs.js`:
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`), then fetch name/year
//!    (`getTmdbNameAndYear`).
//! 2. Emit the three static embed URLs (upstream `EMBED_SOURCES`):
//!    Pro Multi (`web.nxsha.app`), Cinesrc (`cinesrc.st`), and 4K
//!    (`player.videasy.net`). Upstream's doc notes the site's embed pages
//!    carry a CFG object with server URLs keyed by TMDB ids — the
//!    provider never scrapes it: the same three URLs are constructed
//!    statically (the CFG/packed-JS decoding of those pages belongs to
//!    the `vidsrc`/`videasy`-family extractors, and to
//!    [`vsources_core::unpack`]).
//! 3. Resolve each embed through the [`ExtractorRegistry`]. The JS passed
//!    `meta.vidking` for **both** movies and series — the speedracelight
//!    API resolves TV by exact tmdb id + season + episode (verified live:
//!    correct series content through this exact path; the old
//!    "wrong content for series" note dates from the title-fuzzy
//!    matching era) — so the extract context carries media with a TMDB
//!    id and the season/episode context for TV, always. Without it the
//!    TV embeds (`web.nxsha.app`, `cinesrc.st`, `player.videasy.net`)
//!    match no dedicated extractor and the source returns zero series
//!    streams.
//!
//! Cuts from the upstream, mapped rather than dropped:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{name} ({year})` / `{name} S01E02`, plus the server label) is
//!   carried as [`Stream::label`] on every resolved stream.
//! - `meta.vidking` — the registry joins the `vidking` extractor when the
//!   extract context carries media with a TMDB id (always here).
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
const ID: &str = "vidsrcsbs";
/// The display label, upstream `this.label`.
const LABEL: &str = "VidSrcSbs";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://vidsrc.sbs";

/// One static server, upstream `EMBED_SOURCES`.
struct EmbedSource {
    /// Card label.
    label: &'static str,
    /// Movie embed template.
    movie: &'static str,
    /// TV embed template.
    tv: &'static str,
}

/// The three servers, in upstream order.
const EMBED_SOURCES: &[EmbedSource] = &[
    EmbedSource {
        label: "Pro Multi",
        movie: "https://web.nxsha.app/embed/movie/{id}",
        tv: "https://web.nxsha.app/embed/tv/{id}/{s}/{e}?server=AwsPly-[Multi-Lang]",
    },
    EmbedSource {
        label: "Cinesrc",
        movie: "https://cinesrc.st/embed/movie/{id}",
        tv: "https://cinesrc.st/embed/tv/{id}?s={s}&e={e}&color=FF1493&autoplay=true&autonext=true",
    },
    EmbedSource {
        label: "4K",
        movie: "https://player.videasy.net/movie/{id}",
        tv: "https://player.videasy.net/tv/{id}/{s}/{e}",
    },
];

/// The `VidSrcSbs` provider.
pub struct VidSrcSbs {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The registry the embeds resolve through.
    extractors: Arc<ExtractorRegistry>,
    /// TMDB identity and metadata resolution.
    tmdb: Arc<TmdbClient>,
}

impl VidSrcSbs {
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
impl Source for VidSrcSbs {
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

        // `meta.vidking` for both kinds — TV carries the exact
        // season/episode the API keys on (`season: Number(tmdbId.season),
        // episode: Number(tmdbId.episode || 1)`).
        let extract_media = ResolvedMedia {
            tmdb_id: Some(tmdb_id),
            imdb_id: None,
            name: name.clone(),
            year,
            season: is_tv.then_some(season),
            episode: is_tv.then_some(episode),
        };

        let mut streams = Vec::new();
        for source in EMBED_SOURCES {
            let raw = if is_tv {
                render(source.tv, tmdb_id, season, episode)
            } else {
                render(source.movie, tmdb_id, season, episode)
            };
            let embed = embed_url(&raw)?;
            let sub_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: Some(extract_media.clone()),
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
            let label = format!("{title} ({})", source.label);
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

    use super::VidSrcSbs;

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
        episode: Option<u32>,
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
                    name: ctx.media.as_ref().map(|media| media.name.clone()),
                    season: ctx.media.as_ref().and_then(|media| media.season),
                    episode: ctx.media.as_ref().and_then(|media| media.episode),
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
    fn provider(mock: &Arc<ScriptedFetcher>, stubs: &[Arc<StubExtractor>]) -> VidSrcSbs {
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
        VidSrcSbs::new(Arc::new(extractors), Arc::new(tmdb))
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
        assert_eq!(info.id, "vidsrcsbs");
        assert_eq!(info.label, "VidSrcSbs");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://vidsrc.sbs/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn emits_the_three_movie_embeds() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/movie/27205",
            r#"{"title":"Inception","release_date":"2010-07-16"}"#,
        ));
        let generic = StubExtractor::build(
            "generic",
            &["nxsha.app", "cinesrc.st", "videasy.net"],
            Outcome::Direct,
        );
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic.clone(), vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        assert_eq!(streams.len(), 3);
        // Every embed resolved to the stub's direct stream; the embeds
        // themselves are what the stub recorded, in server order.
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
                "https://web.nxsha.app/embed/movie/27205",
                "https://cinesrc.st/embed/movie/27205",
                "https://player.videasy.net/movie/27205",
            ]
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Inception (2010) (Pro Multi)")
        );
        assert_eq!(
            streams[1].label.as_deref(),
            Some("Inception (2010) (Cinesrc)")
        );
        assert_eq!(streams[2].label.as_deref(), Some("Inception (2010) (4K)"));
        for stream in &streams {
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
            assert_eq!(stream.meta.source_id.as_deref(), Some("vidsrcsbs"));
            assert_eq!(stream.meta.source_label.as_deref(), Some("VidSrcSbs"));
        }
        // `meta.vidking` for movies too — but the dedicated extractor
        // already resolved, so the fallback never joins the chain.
        assert_eq!(generic.calls().len(), 3);
        assert!(vidking.calls().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn emits_the_three_series_embeds_with_season_context() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/tv/1396",
            r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
        ));
        let generic = StubExtractor::build(
            "generic",
            &["nxsha.app", "cinesrc.st", "videasy.net"],
            Outcome::Miss,
        );
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic, vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        // Every embed missed its dedicated extractor; the media fallback
        // resolved all three with the exact season/episode context.
        assert_eq!(streams.len(), 3);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 (Pro Multi)")
        );
        let calls = vidking.calls();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://web.nxsha.app/embed/tv/1396/1/2?server=AwsPly-[Multi-Lang]",
                "https://cinesrc.st/embed/tv/1396?s=1&e=2&color=FF1493&autoplay=true&autonext=true",
                "https://player.videasy.net/tv/1396/1/2",
            ]
        );
        for call in &calls {
            assert_eq!(call.tmdb_id, Some(1396));
            assert_eq!(call.season, Some(1));
            assert_eq!(call.episode, Some(2));
            assert_eq!(call.source_id.as_deref(), Some("vidsrcsbs"));
        }
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
