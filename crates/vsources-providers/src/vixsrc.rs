//! `VixSrc`'s current signed player API, with required English audio selection.
//!
//! Resolve `/api/movie/{tmdb}` or `/api/tv/{tmdb}/{season}/{episode}`, fetch the
//! returned same-origin embed, and parse its `window.masterPlaylist` object.
//! The HLS master must advertise English audio. Signed URLs retain browser
//! playback headers and a bounded ten-minute source-cache lifetime.
//! Reference: J0hnBloodborne/Nautilus, `src/providers/sources/vixsrc.py`;
//! confirmed against the live player on 2026-10-02.

use fancy_regex::Regex;
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    AudioSelection, CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream,
};

/// The provider id, upstream `this.id`.
const ID: &str = "vixsrc";
/// The display label, upstream `this.label`.
const LABEL: &str = "VixSrc";
/// The stream origin, upstream `ORIGIN`/`this.baseUrl`.
const ORIGIN: &str = "https://vixsrc.to";
/// Keep minted URLs fresh; the anonymous embed expires independently.
const TTL: Duration = Duration::from_secs(600);
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
static PLAYER_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:["']?(url|token|expires)["']?)\s*:\s*["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid VixSrc player pattern: {e}"))
});

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
                country_codes: vec![CountryCode::En],
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
        let path = match media.season {
            Some(season) => format!("/api/tv/{tmdb_id}/{season}/{}", media.episode.unwrap_or(1)),
            None => format!("/api/movie/{tmdb_id}"),
        };
        let origin = Url::parse(ORIGIN).map_err(|e| SourceError::scrape(ID, e.to_string()))?;
        let api = origin
            .join(&path)
            .map_err(|e| SourceError::scrape(ID, e.to_string()))?;
        let response = fetch(ctx, api).await?;
        let payload: Value = serde_json::from_str(&response)
            .map_err(|_| SourceError::scrape(ID, "invalid player API response"))?;
        let Some(src) = payload.get("src").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let embed = origin
            .join(src)
            .map_err(|_| SourceError::scrape(ID, "invalid embed URL"))?;
        if embed.origin() != origin.origin() {
            return Err(SourceError::scrape(ID, "unexpected embed origin"));
        }
        let html = fetch(ctx, embed).await?;
        let Some(url) = signed_playlist(&html) else {
            return Ok(Vec::new());
        };
        let playlist = fetch(ctx, url.clone()).await?;
        let Some(audio_index) = english_audio_index(&playlist) else {
            return Ok(Vec::new());
        };
        let mut stream = Stream::new(url, Format::Hls)
            .with_label(title)
            .with_ttl(TTL);
        stream.meta = stream
            .meta
            .with_header("Referer", format!("{ORIGIN}/"))
            .with_header("Origin", ORIGIN)
            .with_header("User-Agent", UA);
        stream.meta.audio_selection = Some(AudioSelection {
            language: CountryCode::En,
            audio_index,
        });
        Ok(vec![with_source(stream, ID, LABEL)])
    }
}

async fn fetch(ctx: &ResolveCtx<'_>, url: Url) -> Result<String, SourceError> {
    let response = soften(
        ctx.fetcher
            .request(
                FetchRequest::get(url)
                    .with_header("Referer", format!("{ORIGIN}/"))
                    .with_header("Origin", ORIGIN)
                    .with_header("User-Agent", UA)
                    .with_timeout(Duration::from_secs(10)),
            )
            .await
            .map_err(SourceError::Fetch),
    )?;
    if matches!(response.status, 404 | 410) {
        return Err(SourceError::NotFound);
    }
    if !response.is_success() {
        return Err(SourceError::scrape(
            ID,
            format!("player HTTP {}", response.status),
        ));
    }
    Ok(response.body)
}

fn signed_playlist(html: &str) -> Option<Url> {
    let player = html
        .split_once("window.masterPlaylist")?
        .1
        .split_once("};")?
        .0;
    let mut fields = std::collections::BTreeMap::new();
    for captures in PLAYER_FIELD.captures_iter(player).flatten() {
        fields.insert(captures.get(1)?.as_str(), captures.get(2)?.as_str());
    }
    let mut url = Url::parse(fields.get("url")?).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("vixsrc.to") {
        return None;
    }
    if !url
        .path()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("m3u8"))
    {
        url.set_path(&format!("{}.m3u8", url.path()));
    }
    url.query_pairs_mut()
        .append_pair("token", fields.get("token")?)
        .append_pair("expires", fields.get("expires")?)
        .append_pair("h", "1")
        .append_pair("lang", "en");
    Some(url)
}

fn english_audio_index(playlist: &str) -> Option<u32> {
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return None;
    }
    for (index, line) in playlist
        .lines()
        .filter(|line| line.starts_with("#EXT-X-MEDIA:") && line.contains("TYPE=AUDIO"))
        .enumerate()
    {
        if line.contains("LANGUAGE=\"eng\"") || line.contains("LANGUAGE=\"en\"") {
            return u32::try_from(index).ok();
        }
    }
    None
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
    stream.meta.languages = vec![CountryCode::En];
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
    use vsources_core::types::MediaId;

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
        assert_eq!(info.country_codes, vec![CountryCode::En]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://vixsrc.to/")
        );
        assert_eq!(info.priority, 1);
        assert_eq!(info.domain_key, None);
    }

    fn live_shape(mock: ScriptedFetcher, api: &str) -> ScriptedFetcher {
        mock.page(api,200,r#"{"src":"/embed/7?token=api-token"}"#)
            .page("/embed/7",200,r"window.masterPlaylist = { params: { 'token': 'media-token', 'expires': '2000000000' }, url: 'https://vixsrc.to/playlist/7?ub=1' };")
            .page("/playlist/7.m3u8",200,"#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"ita\",URI=\"it.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"eng\",URI=\"en.m3u8\"")
    }

    #[tokio::test]
    async fn resolves_fresh_signed_movie_and_selects_english() -> Result<(), SourceError> {
        let mock = Arc::new(live_shape(
            ScriptedFetcher::default().page(
                "/3/movie/27205",
                200,
                r#"{"title":"Inception","release_date":"2010-07-16"}"#,
            ),
            "/api/movie/27205",
        ));
        let streams = provider(&mock)
            .resolve(
                &ctx_for(&mock, None),
                &MediaRef::movie(MediaId::Tmdb(27205)),
            )
            .await?;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.path(), "/playlist/7.m3u8");
        assert_eq!(
            streams[0]
                .url
                .query_pairs()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.into_owned()),
            Some("media-token".into())
        );
        assert_eq!(
            streams[0].meta.audio_selection,
            Some(AudioSelection {
                language: CountryCode::En,
                audio_index: 1
            })
        );
        assert_eq!(streams[0].label.as_deref(), Some("Inception (2010)"));
        assert!(
            mock.requests()
                .iter()
                .filter(|r| r.url.host_str() == Some("vixsrc.to"))
                .all(|r| r.headers.contains_key("User-Agent"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_preserves_exact_season_episode() -> Result<(), SourceError> {
        let mock = Arc::new(live_shape(
            ScriptedFetcher::default().page(
                "/3/tv/1396",
                200,
                r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#,
            ),
            "/api/tv/1396/2/1",
        ));
        let streams = provider(&mock)
            .resolve(
                &ctx_for(&mock, None),
                &MediaRef::series(MediaId::Tmdb(1396), 2, 1),
            )
            .await?;
        assert_eq!(streams.len(), 1);
        assert!(
            mock.requests()
                .iter()
                .any(|r| r.url.path() == "/api/tv/1396/2/1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_catalog_entry_does_not_fabricate_a_stream() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/3/movie/27205", 200, r#"{"title":"Inception"}"#)
                .page("/api/movie/27205", 404, "not found"),
        );
        let result = provider(&mock)
            .resolve(
                &ctx_for(&mock, None),
                &MediaRef::movie(MediaId::Tmdb(27205)),
            )
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[test]
    fn ignores_unrelated_tokens_and_rejects_non_english_or_html() {
        assert!(signed_playlist("var token='not a playlist';").is_none());
        assert!(
            signed_playlist(
                "window.masterPlaylist = {url:'https://other.example/p',token:'x',expires:'1'};"
            )
            .is_none()
        );
        assert_eq!(
            english_audio_index("#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"ita\""),
            None
        );
        assert_eq!(english_audio_index("<html>LANGUAGE=\"eng\"</html>"), None);
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
