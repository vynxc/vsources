//! `CineWave`: `HdHub` direct streams plus a 22-embed fallback fan.
//!
//! Ports `src/source/CineWave.js` (`watch.cinewave.qzz.io` — movies,
//! series, anime, K-drama). Two layers:
//!
//! 1. **`HdHub` API** (`hdhub.thevolecitor.qzz.io`, free, keyed by `IMDb` ids —
//!    TMDB ids must not be used, the API answers them with the donation
//!    stream only): `/{config}/stream/{movie|series}/{imdb}.json` returns
//!    up to ~35 direct CDN URLs (FSL, Pixeldrain, Cloudflare R2, `HubCDN`,
//!    Jio workers). Every upstream filter is kept: donation cards are
//!    skipped, expired Cloudflare R2 pre-signed URLs are dropped (the
//!    `X-Amz-Date` + `X-Amz-Expires` signature check — the API sometimes
//!    serves stale signatures that 403 on playback), streams whose text
//!    belongs to a different title (normalized containment) or year
//!    (±1) are rejected, and height/size hints come from the `1080p` and
//!    `3.3GB` fragments plus `behaviorHints.videoSize`.
//! 2. **Embed fallback**: 22 embed URLs (upstream `EMBED_SOURCES`
//!    verbatim) resolved through the [`ExtractorRegistry`]. Upstream
//!    handed these to its resolver, which fanned them into
//!    `extractorRegistry.handle`; this port resolves inline. Upstream
//!    attached the `VidKing` speedracelight hint (`meta.vidking`, movies
//!    only — the API fuzzy-matches series wrong), mirrored here by
//!    giving the embed context media with a TMDB id for movies and no
//!    media for series: exactly the routing the registry's media-keyed
//!    fallback implements.
//!
//! Cuts for the library port:
//!
//! - `meta.title` has no `StreamMeta` field — the stream label carries
//!   the cleaned `HdHub` title, and embed results keep the labels their
//!   extractors produce (upstream's `${title} (${label})` card titles
//!   are resolver-level display state; the embed table's labels exist
//!   only for those).
//! - Upstream resolved the embed fan concurrently (`Promise.all`); this
//!   port resolves it sequentially — cross-provider fan-out is the
//!   engine's concurrency domain. A failing embed is dropped, like the
//!   upstream per-extractor `catch`.
//! - Upstream inferred formats (`.m3u8`/`/hls/` → HLS, `.mp4`/`.mkv` →
//!   MP4) in the resolver; providers emit final streams here, so the
//!   same URL-based inference runs in this module.
//! - `HdHub` result caching, `notWebReady` propagation, and the resolver's
//!   client-budget machinery are the parent's `CachedSource`/engine
//!   domain, not this module's.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::language::find_country_codes;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The `HdHub` API root (Stremio-addon format, free, no auth).
const HDHUB_API_BASE: &str = "https://hdhub.thevolecitor.qzz.io";
/// The base64 addon config the API path is keyed by
/// (`{"torbox":"unset","qualities":"2160p,1080p,720p","sort":"desc"}`).
const HDHUB_CONFIG: &str =
    "eyJ0b3Jib3giOiJ1bnNldCIsInF1YWxpdGllcyI6IjIxNjBwLDEwODBwLDcyMHAiLCJzb3J0IjoiZGVzYyJ9";
/// The page the API requires as `Referer`.
const HDHUB_REFERER: &str = "https://watch.cinewave.qzz.io/";

/// One of the 22 fallback embed sources (upstream `EMBED_SOURCES`).
struct EmbedSource {
    /// The movie embed template (`{id}`).
    movie: &'static str,
    /// The series embed template (`{id}`, `{s}`, `{e}`).
    tv: &'static str,
}

/// Every embed `CineWave` falls back to, in upstream order.
const EMBED_SOURCES: &[EmbedSource] = &[
    EmbedSource {
        movie: "https://vidsrc-embed.ru/embed/movie/{id}",
        tv: "https://vidsrc-embed.ru/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://2embed.cc/embed/movie/{id}",
        tv: "https://2embed.cc/embed/tv/{id}&s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://player.vidzee.wtf/embed/movie/{id}",
        tv: "https://player.vidzee.wtf/embed/tv/{id}?season={s}&episode={e}",
    },
    EmbedSource {
        movie: "https://vidfast.pro/movie/{id}",
        tv: "https://vidfast.pro/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://player.videasy.net/movie/{id}",
        tv: "https://player.videasy.net/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://peachify.top/embed/movie/{id}",
        tv: "https://peachify.top/embed/tv/{id}?season={s}&episode={e}",
    },
    EmbedSource {
        movie: "https://cinemaos.tech/embed/movie/{id}",
        tv: "https://cinemaos.tech/embed/tv/{id}?s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://vidcore.net/embed/movie/{id}",
        tv: "https://vidcore.net/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://vidking.net/embed/movie/{id}",
        tv: "https://vidking.net/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://vidlux.xyz/embed/movie/{id}",
        tv: "https://vidlux.xyz/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://hexa.su/embed/movie/{id}",
        tv: "https://hexa.su/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://mappletv.uk/embed/movie/{id}",
        tv: "https://mappletv.uk/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://rivestream.org/embed?type=movie&id={id}",
        tv: "https://rivestream.org/embed?type=tv&id={id}&season={s}&episode={e}",
    },
    EmbedSource {
        movie: "https://airflix1.com/movie/{id}",
        tv: "https://airflix1.com/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://fmovies.gd/movie/{id}",
        tv: "https://fmovies.gd/tv/{id}?s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://111movies.net/movie/{id}",
        tv: "https://111movies.net/tv/{id}?s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://zorostream.com/embed/movie/{id}",
        tv: "https://zorostream.com/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://vidsrc.me/embed/movie?tmdb={id}",
        tv: "https://vidsrc.me/embed/tv?tmdb={id}&season={s}&episode={e}",
    },
    EmbedSource {
        movie: "https://embed.su/embed/movie/{id}",
        tv: "https://embed.su/embed/tv/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://multiembed.mov/?video_id={id}&tmdb=1",
        tv: "https://multiembed.mov/?video_id={id}&tmdb=1&s={s}&e={e}",
    },
    EmbedSource {
        movie: "https://superflixapi.co/filme/{id}",
        tv: "https://superflixapi.co/serie/{id}/{s}/{e}",
    },
    EmbedSource {
        movie: "https://moviesapi.club/movie/{id}",
        tv: "https://moviesapi.club/tv/{id}-{s}-{e}",
    },
];

/// Upstream result lifetime: the source default 12h.
const TTL: Duration = Duration::from_hours(12);
/// The `HdHub` API timeout (upstream: 10s).
const HDHUB_TIMEOUT: Duration = Duration::from_secs(10);

/// The `CineWave` provider.
pub struct CineWave {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The embed resolution chain for the fallback layer.
    extractors: Arc<ExtractorRegistry>,
}

impl CineWave {
    /// A provider over the shared TMDB client and the embed registry.
    pub fn new(tmdb: Arc<TmdbClient>, extractors: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: "cinewave".to_string(),
                label: "CineWave".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Some(
                    Url::parse("https://watch.cinewave.qzz.io")
                        .unwrap_or_else(|e| panic!("valid CineWave base URL: {e}")),
                ),
                priority: 1,
                domain_key: None,
            },
            tmdb,
            extractors,
        }
    }

    /// Layer 1 — the `HdHub` API, fully guarded like the upstream `try`.
    ///
    /// Any failure (no `IMDb` id, network, JSON) simply skips the layer.
    async fn hdhub_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        name: &str,
        year: Option<u16>,
        imdb_id: Option<&str>,
    ) -> Vec<Stream> {
        // `getImdbId` returning null crashed the upstream try block; the
        // port skips the layer instead.
        let Some(imdb) = imdb_id else {
            return Vec::new();
        };
        let is_series = media.season.is_some();
        let stream_id = if is_series {
            format!(
                "{imdb}:{s}:{e}",
                s = media.season.unwrap_or(0),
                e = media.episode.unwrap_or(0)
            )
        } else {
            imdb.to_string()
        };
        let media_type = if is_series { "series" } else { "movie" };
        let target =
            format!("{HDHUB_API_BASE}/{HDHUB_CONFIG}/stream/{media_type}/{stream_id}.json");
        let Ok(url) = Url::parse(&target) else {
            return Vec::new();
        };

        let request = FetchRequest::get(url)
            .with_header("Referer", HDHUB_REFERER)
            .with_header("Accept", "application/json")
            .with_timeout(HDHUB_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(payload) = response.json::<HdHubResponse>() else {
            return Vec::new();
        };

        let now = now_epoch_secs();
        payload
            .streams
            .into_iter()
            .filter_map(|raw| hdhub_stream(raw, name, year, media.season, media.episode, now))
            .collect()
    }

    /// Layer 2 — the embed fan through the extractor registry.
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
impl Source for CineWave {
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
        let imdb_id = imdb_id(ctx, &self.tmdb, media, tmdb_id).await?;

        let mut streams = self
            .hdhub_streams(ctx, media, &name, year, imdb_id.as_deref())
            .await;
        streams.extend(self.embed_streams(ctx, media, tmdb_id, &name, year).await);

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

/// The `IMDb` id for the reference — the id itself when IMDb-keyed, the
/// pre-resolved metadata's id when present, `external_ids` otherwise.
async fn imdb_id(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<Option<String>, SourceError> {
    if let Some(imdb) = media.id.as_imdb() {
        return Ok(Some(imdb.to_string()));
    }
    if let Some(resolved) = &ctx.media
        && let Some(imdb) = &resolved.imdb_id
    {
        return Ok(Some(imdb.clone()));
    }
    soften(tmdb.imdb_id_from_tmdb(tmdb_id, media.kind).await)
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

/// The `HdHub` API response envelope.
#[derive(Deserialize)]
struct HdHubResponse {
    /// The addon's stream cards.
    #[serde(default)]
    streams: Vec<HdHubStream>,
}

/// One `HdHub` stream card.
#[derive(Deserialize)]
struct HdHubStream {
    /// The server label (e.g. `HdHub 1080p`).
    #[serde(default)]
    name: Option<String>,
    /// The release-name blurb (title, size, language).
    #[serde(default)]
    description: Option<String>,
    /// A clean title, when the API ships one.
    #[serde(default)]
    title: Option<String>,
    /// The direct CDN URL.
    #[serde(default)]
    url: Option<String>,
    /// An external (non-playable) URL, e.g. donation cards.
    #[serde(rename = "externalUrl", default)]
    external_url: Option<String>,
    /// Stremio-style behavior hints.
    #[serde(rename = "behaviorHints", default)]
    behavior_hints: Option<HdHubBehaviorHints>,
}

/// The behavior hints `CineWave` reads.
#[derive(Deserialize)]
struct HdHubBehaviorHints {
    /// The byte size of the video file.
    #[serde(rename = "videoSize", default)]
    video_size: Option<u64>,
}

/// Build one `HdHub` stream, applying every upstream filter; `None` skips.
fn hdhub_stream(
    raw: HdHubStream,
    name: &str,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
    now: i64,
) -> Option<Stream> {
    let name_title = format!(
        "{} {}",
        raw.name.as_deref().unwrap_or_default(),
        raw.description.as_deref().unwrap_or_default()
    );

    // Donation cards are not content.
    if contains_donation(&name_title) {
        return None;
    }

    let raw_url = raw.url.as_deref().or(raw.external_url.as_deref())?;
    let url = Url::parse(raw_url).ok()?;

    // Expired Cloudflare R2 pre-signed URLs 403 on playback — skip them.
    if r2_expired(&url, now) {
        return None;
    }

    // The combined text the upstream title/year filters read.
    let stream_text = format!("{name_title} {}", raw.title.as_deref().unwrap_or_default());
    let stream_lower = stream_text.to_lowercase();

    // Title filter — only when the stream text is substantial enough to
    // carry a real title, not just a quality label.
    let name_normalized = normalize(name);
    let stream_normalized = normalize(&stream_lower);
    if name_normalized.len() > 3
        && stream_normalized.len() > 30
        && !stream_normalized.contains(&name_normalized)
    {
        let head = stream_normalized
            .split(' ')
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        if !name_normalized.contains(&head) {
            return None;
        }
    }

    // Year filter — a year more than ±1 from the TMDB year is a
    // different movie with the same title.
    if let Some(year) = year {
        for stream_year in four_digit_years(&stream_lower) {
            let delta = i32::from(stream_year) - i32::from(year);
            if delta.abs() > 1 {
                return None;
            }
        }
    }

    let height = height_hint(&name_title);
    let size = raw
        .behavior_hints
        .as_ref()
        .and_then(|hints| hints.video_size)
        .or_else(|| size_hint(&name_title));

    // Strip the "HdHub" branding — these are CineWave streams. The
    // upstream fallback chain is `stream.title || nameTitle.trim() ||
    // title`.
    let cleaned = raw
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| name_title.trim().to_string());
    let label = if cleaned.trim().is_empty() {
        display_title(name, year, season, episode)
    } else {
        strip_hdhub_branding(&cleaned)
    };

    let mut stream = Stream::new(url.clone(), infer_format(&url)).with_ttl(TTL);
    stream.label = Some(label);
    stream.meta.resolution = height;
    stream.meta.size = size;
    stream.meta.languages = vec![CountryCode::Multi];
    stream
        .meta
        .languages
        .extend(find_country_codes(&name_title));
    stream.meta.source_id = Some("cinewave".to_string());
    stream.meta.source_label = Some("CineWave".to_string());
    Some(stream)
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

/// Whether the text mentions donations (upstream `/donation|donate/i`).
fn contains_donation(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("donation") || lower.contains("donate")
}

/// The upstream normalizer: lowercase, drop `[^a-z0-9\s]`, collapse
/// whitespace.
fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == ' ' {
            out.push(ch);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every `19xx`/`20xx` token (upstream `\b(19\d{2}|20\d{2})\b`).
fn four_digit_years(text: &str) -> Vec<u16> {
    let bytes = text.as_bytes();
    let mut years = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let run = &text[start..i];
            if run.len() == 4
                && (run.starts_with("19") || run.starts_with("20"))
                && let Ok(year) = run.parse::<u16>()
            {
                years.push(year);
            }
        } else {
            i += 1;
        }
    }
    years
}

/// The `(\d{3,})p` height hint.
fn height_hint(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len()
                && (bytes[i] == b'p' || bytes[i] == b'P')
                && let Ok(height) = text[start..i].parse::<u16>()
            {
                return Some(height);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// The `([\d.]+)\s*(GB|MB)` size hint, in bytes (exact integer math).
fn size_hint(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let mut unit = i;
            while unit < bytes.len() && (bytes[unit] == b' ' || bytes[unit] == b'\t') {
                unit += 1;
            }
            let rest = text[unit..].to_lowercase();
            let multiplier = if rest.starts_with("gb") {
                1024u128 * 1024 * 1024
            } else if rest.starts_with("mb") {
                1024u128 * 1024
            } else {
                continue;
            };
            if let Some(size) = decimal_to_bytes(&text[start..i], multiplier) {
                return Some(size);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// `number` × `multiplier`, decimal-scaled exactly.
fn decimal_to_bytes(number: &str, multiplier: u128) -> Option<u64> {
    let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    let mantissa: u128 = format!("{integer}{fraction}").parse().ok()?;
    let scale = 10u128.pow(u32::try_from(fraction.len()).ok()?);
    u64::try_from(mantissa * multiplier / scale).ok()
}

/// Strip the `HdHub ` / `HdHub / VM ` branding prefixes, in upstream
/// replace order.
fn strip_hdhub_branding(title: &str) -> String {
    let mut cleaned = title.to_string();
    if let Some(rest) = strip_prefix_ci(&cleaned, "hdhub")
        && let Some(rest) = strip_leading_spaces(rest)
    {
        cleaned = rest.to_string();
    }
    if let Some(rest) = strip_prefix_ci(&cleaned, "hdhub") {
        let rest = strip_leading_spaces(rest).unwrap_or(rest);
        if let Some(rest) = strip_prefix_ci(rest, "/") {
            let rest = strip_leading_spaces(rest).unwrap_or(rest);
            if let Some(rest) = strip_prefix_ci(rest, "vm")
                && let Some(rest) = strip_leading_spaces(rest)
            {
                return rest.to_string();
            }
        }
    }
    cleaned
}

/// `text` minus a case-insensitive `prefix`, when it starts with it.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `text` minus its leading spaces (`\s+`).
fn strip_leading_spaces(text: &str) -> Option<&str> {
    let rest = text.trim_start_matches([' ', '\t']);
    (rest.len() < text.len()).then_some(rest)
}

/// Whether this is an expired Cloudflare R2 pre-signed URL — `false` for
/// non-R2 URLs; unparsable dates pass through (best effort, like the
/// upstream `catch {}`).
fn r2_expired(url: &Url, now: i64) -> bool {
    if !(url.as_str().contains("r2.cloudflarestorage.com") || url.as_str().contains(".r2.dev")) {
        return false;
    }
    let mut amz_date = None;
    let mut amz_expires = 0u64;
    for (key, value) in url.query_pairs() {
        if key == "X-Amz-Date" {
            amz_date = Some(value.into_owned());
        } else if key == "X-Amz-Expires" {
            amz_expires = value.parse().unwrap_or(0);
        }
    }
    let Some(date) = amz_date else {
        return false;
    };
    let Some(expires) = i64::try_from(amz_expires).ok() else {
        return false;
    };
    if expires <= 0 {
        return false;
    }
    amz_epoch(&date).is_some_and(|signed| now > signed + expires)
}

/// The epoch seconds of an AWS `YYYYMMDDTHHMMSSZ` date.
fn amz_epoch(date: &str) -> Option<i64> {
    let bytes = date.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = date.get(range)?;
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let days = days_from_civil(digits(0..4)?, digits(4..6)?, digits(6..8)?)?;
    let seconds_of_day = digits(9..11)? * 3_600 + digits(11..13)? * 60 + digits(13..15)?;
    Some(days * 86_400 + seconds_of_day)
}

/// Days since the Unix epoch (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The current epoch time in seconds.
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The upstream resolver's format inference for unset formats.
fn infer_format(url: &Url) -> Format {
    let lower = url.as_str().to_ascii_lowercase();
    if lower.contains(".m3u8")
        || lower.contains("/m3u8/")
        || lower.contains("/hls/")
        || lower.contains("/playlist/")
    {
        Format::Hls
    } else if lower.contains(".mp4") || lower.contains(".mkv") {
        Format::Mp4
    } else {
        Format::Unknown
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
    use vsources_core::traits::{Extractor, FetchResponse, Fetcher};

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

        /// The value of a header sent to `key` (host + path).
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| {
                    let host = request.url.host_str().unwrap_or_default();
                    format!("{host}{}", request.url.path()) == key
                })
                .and_then(|request| {
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

    /// A resolved Dune movie.
    fn dune_media() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: Some("tt1160419".to_string()),
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

    /// The `HdHub` mock key for a movie id.
    fn hdhub_key(id: &str) -> String {
        format!("hdhub.thevolecitor.qzz.io/{HDHUB_CONFIG}/stream/movie/{id}.json")
    }

    /// The TMDB mock key for a movie path.
    fn tmdb_key(path: &str) -> String {
        format!("api.themoviedb.org{path}")
    }

    /// A provider over an empty extractor registry (embeds yield nothing).
    fn provider(fetcher: &Arc<MockFetcher>) -> CineWave {
        CineWave::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(Vec::new())),
        )
    }

    #[tokio::test]
    async fn filters_donation_expired_and_mismatched_streams() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "streams": [
                {"name": "🌟 Donation needed.", "description": "Click here to donate to hdhub.", "externalUrl": "http://hdhub.example/donation.html"},
                {"name": "HdHub 1080p", "description": "[10Gbps] [💾 3.3GB] Dune.2021.1080p.WEB-DL.Hindi-English.DD5.1.x264.mkv\nHindi\nEnglish", "url": "https://pixel.hubcloud.ist/?id=aaa", "behaviorHints": {"notWebReady": true, "videoSize": 3_543_348_019u64}},
                {"name": "HdHub 720p", "description": "[💾 1.3GB] Dune.2021.720p.Hindi.English.x264.mkv", "url": "https://files.jiomovies.workers.dev/dune-720p"},
                {"name": "HdHub 1080p", "description": "[💾 2.2GB] Eye.for.an.Eye.2025.1080p.x264.mkv", "url": "https://cdn.example/eye"},
                {"name": "HdHub 2160p", "description": "[💾 8GB] Totally.Unrelated.Movie.1999.2160p.mkv", "url": "https://cdn.example/other"},
                {"name": "HdHub 1080p", "description": "[💾 2GB] Dune.2021.1080p.x264.mkv", "url": "https://bucket.r2.dev/video/file?X-Amz-Date=20200101T000000Z&X-Amz-Expires=3600"}
            ]
        });
        let key = hdhub_key("tt1160419");
        let fetcher = Arc::new(MockFetcher::new().serve(&key, Scripted::json(&payload)));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the HdHub fixture must resolve: {e}"));
        assert_eq!(
            streams.len(),
            2,
            "donation, wrong-title, and expired-R2 cards must all drop"
        );

        let first = &streams[0];
        assert_eq!(first.url.as_str(), "https://pixel.hubcloud.ist/?id=aaa");
        assert_eq!(first.format, Format::Unknown);
        assert_eq!(first.meta.resolution, Some(1080));
        assert_eq!(first.meta.size, Some(3_543_348_019));
        assert_eq!(first.meta.source_id.as_deref(), Some("cinewave"));
        assert_eq!(first.meta.source_label.as_deref(), Some("CineWave"));
        // Upstream strips only the `HdHub ` branding prefix, so the
        // quality label stays at the head of the title.
        assert!(
            first
                .label
                .as_deref()
                .is_some_and(|label| label.starts_with("1080p [10Gbps]"))
        );

        let second = &streams[1];
        assert_eq!(second.meta.resolution, Some(720));
        assert_eq!(second.meta.size, Some(1_395_864_371));
        // Upstream iterates its language map (en before hi), so English
        // precedes Hindi regardless of text order.
        assert_eq!(
            second.meta.languages,
            vec![CountryCode::Multi, CountryCode::En, CountryCode::Hi]
        );

        assert_eq!(
            fetcher.sent_header(&key, "Referer").as_deref(),
            Some("https://watch.cinewave.qzz.io/")
        );
        assert_eq!(
            fetcher.sent_header(&key, "Accept").as_deref(),
            Some("application/json")
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_requests_the_season_episode_stream_id() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "streams": [
                {"name": "HdHub 1080p", "description": "[💾 2GB] Breaking Bad S01E02 1080p English x264.mkv", "url": "https://cdn.example/bb"}
            ]
        });
        let key =
            format!("hdhub.thevolecitor.qzz.io/{HDHUB_CONFIG}/stream/series/tt0903747:1:2.json");
        let fetcher = Arc::new(MockFetcher::new().serve(&key, Scripted::json(&payload)));
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: Some("tt0903747".to_string()),
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(1),
                episode: Some(2),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 1, 2);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), "https://cdn.example/bb");
        Ok(())
    }

    #[tokio::test]
    async fn movie_embeds_resolve_through_the_registry() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    &tmdb_key("/3/movie/438631"),
                    Scripted::json(
                        &serde_json::json!({"title": "Dune", "release_date": "2021-10-22"}),
                    ),
                )
                .serve(
                    &tmdb_key("/3/movie/438631/external_ids"),
                    Scripted::json(&serde_json::json!({"imdb_id": "tt1160419"})),
                ),
        );
        let fake = Arc::new(FakeExtractor::new());
        let provider = CineWave::new(
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
                .all(|(_, source, _)| source.as_deref() == Some("cinewave"))
        );
        assert!(seen.iter().all(|(_, _, tmdb)| *tmdb == Some(438_631)));
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://vidsrc-embed.ru/embed/movie/438631")
        );
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://vidsrc.me/embed/movie?tmdb=438631")
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_embeds_carry_no_media_and_use_season_templates() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    &tmdb_key("/3/tv/1396"),
                    Scripted::json(&serde_json::json!({"name": "Breaking Bad", "first_air_date": "2008-01-20"})),
                )
                .serve(
                    &tmdb_key("/3/tv/1396/external_ids"),
                    Scripted::json(&serde_json::json!({"imdb_id": "tt0903747"})),
                ),
        );
        let fake = Arc::new(FakeExtractor::new());
        let provider = CineWave::new(
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
            seen.iter()
                .any(|(url, _, _)| *url == "https://vidsrc-embed.ru/embed/tv/1396/2/3")
        );
        assert!(
            seen.iter()
                .any(|(url, _, _)| *url == "https://2embed.cc/embed/tv/1396&s=2&e=3")
        );
        Ok(())
    }

    #[tokio::test]
    async fn imdb_keyed_references_resolve_through_tmdb_find() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "streams": [
                {"name": "HdHub 1080p", "description": "[💾 2GB] Dune.2021.1080p.English.x264.mkv", "url": "https://cdn.example/dune"}
            ]
        });
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
                )
                .serve(&hdhub_key("tt1160419"), Scripted::json(&payload)),
        );
        let ctx = ctx_for(&fetcher, None);
        let media = MediaRef::imdb("tt1160419", MediaType::Movie);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the IMDb-keyed reference must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), "https://cdn.example/dune");
        Ok(())
    }

    #[tokio::test]
    async fn nothing_resolvable_is_not_found() {
        let fetcher = Arc::new(MockFetcher::new());
        let ctx = ctx_for(&fetcher, None);

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a total miss must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failing_hdhub_api_only_leaves_the_embed_fan() -> Result<(), SourceError> {
        let fake = Arc::new(FakeExtractor::new());
        let fetcher = Arc::new(MockFetcher::new());
        let provider = CineWave::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(ExtractorRegistry::new(vec![fake])),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the embed fan must survive a dead HdHub: {e}"));
        assert_eq!(streams.len(), EMBED_SOURCES.len());
        Ok(())
    }
}
