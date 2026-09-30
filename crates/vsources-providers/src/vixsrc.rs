//! `VixSrc`: the vixsrc.to playlist contract, one player-side card.
//!
//! Ports `src/source/VixSrc.js` (`vixsrc.to` — movies and series). The
//! upstream ships exactly one stream per resolve: the site's player JS
//! builds `https://vixsrc.to/api/playlist/{tmdbId}?token=` (movies) or
//! `/api/playlist/{tmdbId}/{season}/{episode}?token=` (series) with an
//! empty free-tier token, and the card plays from the **player's**
//! residential IP — the host Cloudflare-blocks datacenter egress, so
//! server-side validation was advisory-only upstream and is dropped
//! entirely here.
//!
//! Cuts for the library port:
//!
//! - `meta.title` has no `StreamMeta` field — the JS `${name} (${year})`
//!   / `${name} S01E02` title string becomes [`Stream::label`].
//! - The `nuvioReferer`/`nuvioOrigin`/`nuvioDirectWithHeaders` meta
//!   flags existed so the server's extractor shipped the playlist URL
//!   through its `/proxy` with headers attached. There is no server
//!   here, so the same headers ride
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!   (`Referer`/`Origin: vixsrc.to`) — the direct-with-headers policy
//!   the flags encoded.
//! - The stream is never validated (the upstream never validated it
//!   either; a premium-title 404 is an availability gap, not an error).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

/// The provider id, upstream `this.id`.
const ID: &str = "vixsrc";
/// The display label, upstream `this.label`.
const LABEL: &str = "VixSrc";
/// The stream origin, upstream `ORIGIN`/`this.baseUrl`.
const ORIGIN: &str = "https://vixsrc.to";
/// Upstream sets no `this.ttl` — the source default of 12h.
const TTL: Duration = Duration::from_hours(12);

/// The `VixSrc` provider.
pub struct VixSrc {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl VixSrc {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse(ORIGIN).ok(),
                // Upstream `this.priority = 1`.
                priority: 1,
                domain_key: None,
            },
            tmdb,
        }
    }
}

#[async_trait]
impl Source for VixSrc {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;

        let title = display_title(&name, year, media);
        let url = playlist_url(tmdb_id, media.season, media.episode)?;

        // The single player-IP card: direct playlist URL with the
        // browser headers the site's own player sends.
        let mut stream = Stream::new(url, Format::Hls)
            .with_label(title)
            .with_ttl(TTL);
        stream.meta = stream
            .meta
            .with_header("Referer", format!("{ORIGIN}/"))
            .with_header("Origin", ORIGIN);
        Ok(vec![with_source(stream, ID, LABEL)])
    }
}

/// The playlist URL — `?token=` stays empty for free titles (the site
/// player builds exactly this URL); a parse failure is a structural
/// surprise.
fn playlist_url(
    tmdb_id: u64,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<Url, SourceError> {
    let raw = match (season, episode) {
        (Some(season), episode) => {
            let episode = episode.unwrap_or(1);
            format!("{ORIGIN}/api/playlist/{tmdb_id}/{season}/{episode}?token=")
        }
        _ => format!("{ORIGIN}/api/playlist/{tmdb_id}?token="),
    };
    Url::parse(&raw)
        .map_err(|error| SourceError::scrape(ID, format!("invalid playlist URL `{raw}`: {error}")))
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the upstream
/// `meta.title`, carried as the stream label.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear` (whose errors propagate upstream).
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
    Ok((name.name, name.year))
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`; miss pages map onto
/// [`SourceError::NotFound`] like the upstream `NotFoundError`.
async fn tmdb_id(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
        },
    }
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

/// Attach the provider identity and language flags to a stream.
fn with_source(mut stream: Stream, id: &str, label: &str) -> Stream {
    stream.meta.languages = vec![CountryCode::Multi];
    stream.meta.source_id = Some(id.to_string());
    stream.meta.source_label = Some(label.to_string());
    stream
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::{Format, MediaId};

    use super::*;

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare path,
    /// so TMDB requests (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<(u16, String)>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `status`/`body`; earlier registrations pop
        /// first.
        fn page(self, key: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push((status, body.into()));
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
            let entry = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let entry = match entry {
                Some(entry) => Some(entry),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_mut(request.url.path())
                    .map(|bodies| {
                        if bodies.len() > 1 {
                            bodies.remove(0)
                        } else {
                            bodies[0].clone()
                        }
                    }),
            };
            let Some((status, body)) = entry else {
                return Err(FetchError::NotFound { url: request.url });
            };
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> VixSrc {
        VixSrc::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
    }

    /// A context over the scripted fetcher, optionally with media.
    fn ctx_for(mock: &Arc<ScriptedFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = mock.as_ref();
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
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "vixsrc");
        assert_eq!(info.label, "VixSrc");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://vixsrc.to/")
        );
        assert_eq!(info.priority, 1);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_the_movie_playlist_card() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/movie/27205",
            200,
            r#"{"title":"Inception","release_date":"2010-07-16"}"#,
        ));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://vixsrc.to/api/playlist/27205?token="
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.label.as_deref(), Some("Inception (2010)"));
        assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        assert_eq!(stream.meta.source_id.as_deref(), Some("vixsrc"));
        assert_eq!(stream.meta.source_label.as_deref(), Some("VixSrc"));
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://vixsrc.to/")
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some("https://vixsrc.to")
        );
        assert_eq!(stream.ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_the_series_playlist_card_with_season_and_episode() -> Result<(), SourceError>
    {
        let mock = Arc::new(ScriptedFetcher::default().page(
            "/3/tv/1396",
            200,
            r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
        ));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://vixsrc.to/api/playlist/1396/1/2?token="
        );
        assert_eq!(stream.label.as_deref(), Some("Breaking Bad S01E02"));
        Ok(())
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let media = ResolvedMedia {
            tmdb_id: Some(550),
            imdb_id: None,
            name: "Fight Club".to_string(),
            year: Some(1999),
            season: None,
            episode: None,
        };
        let ctx = ctx_for(&mock, Some(media));

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(550)))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("Fight Club (1999)"));
        assert!(
            mock.requests().is_empty(),
            "no fetches for pre-resolved media"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock, None);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(404)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }
}
