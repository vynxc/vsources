//! `Cineby`: cineby.by through vidking's speedracelight backend.
//!
//! Ports `src/source/Cineby.js` + `src/nuvio/cineby.cjs` (movies, series,
//! anime with multi-quality HLS up to 4K). cineby.by is a Laravel
//! Livewire catalog whose player pages iframe vidking.net embeds, so
//! the playable backend is vidking's `api.speedracelight.com`:
//!
//! 1. `GET /seed?mediaId={tmdbId}` → `{ seed }` — the shared
//!    [`SeedStore`] (the `srlSeed.cjs` singleton: 25 s cache, per-id
//!    coalescing, 120 s edge-5xx down-mark) keeps one seed per sweep;
//!    the API rotates seeds per request, so parallel fetches
//!    invalidate each other.
//! 2. `GET /{endpoint}?title=&mediaType=&year=&episodeId=&seasonId=
//!    tmdbId=&imdbId=&enc=2&seed=&_t=` → a base64+XOR keystream body
//!    (magic `mvm1`) decrypted with [`decrypt_payload`].
//! 3. The wrapper sweeps the bundle's six server tabs — Yoru (`cdn`),
//!    Breach (`m4uhd`), `hdmovie`, Killjoy (`meine`, `language=german`),
//!    Omen (`lamovie`), Raze (`superflix`) — sharing one seed, retrying
//!    once through a seed invalidation on 401/decrypt failure, and
//!    splitting the `hdmovie` payload locally into the bundle's Vyse
//!    (exact quality `English`) / Fade (exact quality `Hindi`) tabs.
//!    `playlist` master URLs become one resilient `Auto` card per
//!    server; cards are deduped by URL and sorted 4K-first; the API's
//!    inline VTT subtitles ride every card of their sweep (the dead
//!    `api.playhq.net` proxy host is skipped).
//! 4. The wrapper retries an empty sweep twice (2 s apart) — stopping
//!    early once the seed endpoint marks the whole API down — all
//!    inside a 30 s race; the raw streams then flow through
//!    [`build_stream_results`] with the vidking hotlink headers
//!    (`Origin`/`Referer: vidking.net` — the peakstorm CDN gate is
//!    inverted for this family) mapped onto
//!    [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).
//!
//! Cuts for the library port:
//!
//! - The TMDB `original_language` probe that chose the audio
//!   `CountryCode` is cut (the shared `TmdbClient` does not expose it —
//!   the `imdbplay`/`itachi` precedent), so the source-level codes are
//!   `[multi]` like the JS's unknown-language fallback; per-card
//!   language flags still arrive via `build_stream_results`' title
//!   scanning (`Fade (Hindi)` → `hi`).
//! - No `/proxy` routing and no result cache: the former becomes
//!   request headers, the latter is the parent `CachedSource`'s
//!   domain. `meta.title` becomes [`Stream::label`].
//! - The JS's per-server **server-side playlist validation** of
//!   salsa436jam.com URLs never existed (the comment documents the
//!   opposite decision), so nothing is dropped here either — cards
//!   ship direct with player-IP + vidking referer semantics.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fancy_regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::speedracelight::{
    ProviderQuery, SPEEDRACELIGHT_API_BASE, SeedStore, decrypt_payload, fetch_seed,
};
use crate::nuvio::{BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline};

/// The provider id, upstream `this.id`.
const ID: &str = "cineby";
/// The display label, upstream `this.label`.
const LABEL: &str = "Cineby";
/// The catalog origin, upstream `this.baseUrl` (embeds live at
/// vidking.net; the hotlink headers below carry the real origin).
const BASE_URL: &str = "https://cineby.by";
/// Upstream `this.ttl` — stream URLs have short-lived tokens.
const TTL: Duration = Duration::from_mins(10);
/// The vidking player the speedracelight API gates its CORS on.
const VIDKING_ORIGIN: &str = "https://www.vidking.net";
/// The hotlink Referer.
const VIDKING_REFERER: &str = "https://www.vidking.net/";
/// The upstream browser UA (`UA`).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// One provider fetch (`AbortSignal.timeout(12_000)`).
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(12);
/// The wrapper's outer race (30 s).
const SWEEP_DEADLINE: Duration = Duration::from_secs(30);
/// Empty-sweep retries after the first (`EMPTY_RETRY_MAX`).
const EMPTY_RETRY_MAX: u32 = 2;
/// Delay between empty sweeps (`EMPTY_RETRY_DELAY_MS`).
const EMPTY_RETRY_DELAY: Duration = Duration::from_secs(2);
/// The 401-retry window — a second full round only while the sweep is
/// young (the JS `t0` gate).
const RETRY_WINDOW: Duration = Duration::from_secs(12);

/// One vidking bundle server tab — the endpoint plus optional extra
/// query parameters (Killjoy's `language=german`).
struct CinebyProvider {
    /// The bundle's server display name.
    name: &'static str,
    /// The API endpoint under [`SPEEDRACELIGHT_API_BASE`].
    endpoint: &'static str,
    /// Extra query parameter appended verbatim.
    extra: Option<(&'static str, &'static str)>,
}

/// The six live server tabs, verbatim from the vidking bundle (Cypher
/// and Neon were dropped upstream as permanently 404).
const CINEBY_PROVIDERS: [CinebyProvider; 6] = [
    CinebyProvider {
        name: "Yoru",
        endpoint: "cdn/sources-with-title",
        extra: None,
    },
    CinebyProvider {
        name: "Breach",
        endpoint: "m4uhd/sources-with-title",
        extra: None,
    },
    CinebyProvider {
        name: "hdmovie",
        endpoint: "hdmovie/sources-with-title",
        extra: None,
    },
    CinebyProvider {
        name: "Killjoy",
        endpoint: "meine/sources-with-title",
        extra: Some(("language", "german")),
    },
    CinebyProvider {
        name: "Omen",
        endpoint: "lamovie/sources-with-title",
        extra: None,
    },
    CinebyProvider {
        name: "Raze",
        endpoint: "superflix/sources-with-title",
        extra: None,
    },
];

/// The endpoint the bundle defines twice (Vyse `English` / Fade
/// `Hindi`) — fetched once and split locally.
const HDMOVIE_ENDPOINT: &str = "hdmovie/sources-with-title";

/// The subtitle proxy host that answers 400 server-side — its URLs
/// would ship dead subtitles.
static DEAD_SUB_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)api\.playhq\.net").unwrap_or_else(|e| panic!("valid dead-sub pattern: {e}"))
});

/// `(\d{3,4})` — the bare number inside a quality label.
static QUALITY_NUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})").unwrap_or_else(|e| panic!("valid quality-number pattern: {e}"))
});

/// The `Cineby` provider.
pub struct Cineby {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The shared speedracelight seed store (the `srlSeed.cjs`
    /// singleton) — share one instance with every other
    /// speedracelight-backed provider.
    seeds: Arc<SeedStore>,
}

impl Cineby {
    /// A provider over the shared TMDB client and seed store.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, seeds: Arc<SeedStore>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            seeds,
        }
    }
}

#[async_trait]
impl Source for Cineby {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // A confirmed-down speedracelight API answers an instant honest
        // zero — skip even the TMDB lookups (self-heals within 120 s).
        if self.seeds.is_down() {
            return Err(SourceError::NotFound);
        }
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        // The API wants the IMDb id on every call — best-effort, like
        // the upstream try/catch.
        let imdb_id = best_effort_imdb(ctx, media, &self.tmdb, tmdb_id).await;
        let title = display_title(&name, year, media);

        let query = ProviderQuery {
            title: name,
            year,
            media_type: if media.season.is_some() {
                "tv".to_string()
            } else {
                "movie".to_string()
            },
            tmdb_id,
            imdb_id,
            season_id: media.season.unwrap_or(1),
            episode_id: media.episode.unwrap_or(1),
        };

        // The sweep ladder inside the 30 s race: one empty sweep is not
        // "no streams" (individual servers 500 stochastically), so
        // retry twice — stopping once the seed endpoint has marked the
        // whole API down.
        let sweep = async {
            let mut streams = self.sweep(ctx, &query).await;
            for _ in 0..EMPTY_RETRY_MAX {
                if !streams.is_empty() || self.seeds.is_down() {
                    break;
                }
                tokio::time::sleep(EMPTY_RETRY_DELAY).await;
                streams = self.sweep(ctx, &query).await;
            }
            streams
        };
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }

        // The original-language probe is cut (see the module docs) —
        // the unknown-language fallback `[multi]`.
        let country_codes = vec![CountryCode::Multi];
        Ok(build_stream_results(&BuildParams {
            streams: &raw,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &country_codes,
            ttl: TTL,
        }))
    }
}

impl Cineby {
    /// One full sweep: every server tab in parallel, sharing one seed,
    /// then the hdmovie split, dedupe, and 4K-first sort — the port of
    /// `cineby.cjs` `getStreams`.
    async fn sweep(&self, ctx: &ResolveCtx<'_>, query: &ProviderQuery) -> Vec<NuvioStream> {
        let start = Instant::now();
        let runs = futures::future::join_all(
            CINEBY_PROVIDERS
                .iter()
                .map(|provider| self.run_provider(ctx, query, provider, start)),
        )
        .await;

        let mut streams: Vec<NuvioStream> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for run in runs.into_iter().flatten() {
            if run.endpoint == HDMOVIE_ENDPOINT {
                // The bundle's Vyse/Fade tabs hit this one endpoint with
                // exact-match quality filters; split locally, same
                // output, half the requests. Leftover languages keep
                // the endpoint family name.
                let (english, hindi, rest): (Vec<_>, Vec<_>, Vec<_>) =
                    partition_hdmovie(run.sources);
                for source in english {
                    push(
                        &mut streams,
                        &mut seen,
                        &source,
                        "Vyse (English)",
                        &run.subtitles,
                        false,
                    );
                }
                for source in hindi {
                    push(
                        &mut streams,
                        &mut seen,
                        &source,
                        "Fade (Hindi)",
                        &run.subtitles,
                        false,
                    );
                }
                for source in rest {
                    push(
                        &mut streams,
                        &mut seen,
                        &source,
                        "hdmovie",
                        &run.subtitles,
                        false,
                    );
                }
                if let Some(master) = run.master {
                    push(
                        &mut streams,
                        &mut seen,
                        &RawSource::auto(master),
                        "hdmovie",
                        &run.subtitles,
                        true,
                    );
                }
                continue;
            }
            for source in run.sources {
                push(
                    &mut streams,
                    &mut seen,
                    &source,
                    run.server_name,
                    &run.subtitles,
                    false,
                );
            }
            // Master playlist (multi-variant m3u8) — one resilient
            // "Auto" card.
            if let Some(master) = run.master {
                push(
                    &mut streams,
                    &mut seen,
                    &RawSource::auto(master),
                    run.server_name,
                    &run.subtitles,
                    true,
                );
            }
        }

        // 4K first — cards render top-down.
        streams.sort_by(|a, b| {
            quality_rank(b.quality.as_deref().unwrap_or(""))
                .cmp(&quality_rank(a.quality.as_deref().unwrap_or("")))
        });
        streams
    }

    /// One provider fetch with the seed-invalidation retry — the port
    /// of `runProvider` (seed endpoint down → `None`; 401/decrypt
    /// failure → invalidate + one fresh-seed retry while the sweep is
    /// young; other failures → `None`).
    async fn run_provider(
        &self,
        ctx: &ResolveCtx<'_>,
        query: &ProviderQuery,
        provider: &CinebyProvider,
        start: Instant,
    ) -> Option<ProviderRun> {
        for attempt in 0u32..2 {
            let seed = fetch_seed(ctx, &self.seeds, query.tmdb_id).await.ok()?;
            let url = provider_url(provider, query, &seed);
            let request = FetchRequest::get(url)
                .with_header("User-Agent", UA)
                .with_header("Origin", VIDKING_ORIGIN)
                .with_header("Referer", VIDKING_REFERER)
                .with_header("Accept", "application/json")
                .with_header("Cache-Control", "no-cache, no-store, must-revalidate")
                .with_header("Pragma", "no-cache")
                .with_timeout(PROVIDER_TIMEOUT);
            let response = ctx.fetcher.request(request).await.ok()?;
            if !response.is_success() {
                if response.status == 401 && attempt == 0 && start.elapsed() < RETRY_WINDOW {
                    self.seeds.invalidate(query.tmdb_id);
                    continue;
                }
                return None;
            }
            // A failed decrypt means a stale/rotated seed — the JS's
            // synthetic 401.
            let Some(decrypted) = decrypt_payload(&response.body, &seed, query.tmdb_id) else {
                if attempt == 0 && start.elapsed() < RETRY_WINDOW {
                    self.seeds.invalidate(query.tmdb_id);
                    continue;
                }
                return None;
            };
            let Ok(json) = serde_json::from_str::<Value>(&decrypted) else {
                return None;
            };
            let sources = json.get("sources").and_then(Value::as_array)?;
            return Some(ProviderRun {
                endpoint: provider.endpoint,
                server_name: provider.name,
                sources: sources
                    .iter()
                    .filter_map(|source| {
                        let url = source.get("url")?.as_str()?;
                        Some(RawSource {
                            url: url.to_string(),
                            quality: source
                                .get("quality")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                        })
                    })
                    .collect(),
                subtitles: map_subtitles(&json),
                master: json
                    .get("playlist")
                    .and_then(Value::as_str)
                    .filter(|playlist| !playlist.is_empty())
                    .map(str::to_string),
            });
        }
        None
    }
}

/// One decrypted provider sweep result.
struct ProviderRun {
    /// The endpoint the run came from.
    endpoint: &'static str,
    /// The bundle's server name.
    server_name: &'static str,
    /// URL-bearing sources.
    sources: Vec<RawSource>,
    /// The sweep's inline subtitles.
    subtitles: Vec<NuvioSubtitle>,
    /// The multi-variant master playlist URL, when the API ships one.
    master: Option<String>,
}

/// One raw `{url, quality}` source of a decrypted payload.
struct RawSource {
    /// The stream URL.
    url: String,
    /// The raw quality label.
    quality: String,
}

impl RawSource {
    /// The `Auto` card for a master playlist.
    fn auto(url: String) -> Self {
        Self {
            url,
            quality: "Auto".to_string(),
        }
    }
}

/// Split hdmovie sources into the bundle's exact-match Vyse/Fade
/// quality tabs and the leftovers.
fn partition_hdmovie(sources: Vec<RawSource>) -> (Vec<RawSource>, Vec<RawSource>, Vec<RawSource>) {
    let mut english = Vec::new();
    let mut hindi = Vec::new();
    let mut rest = Vec::new();
    for source in sources {
        match source.quality.as_str() {
            "English" => english.push(source),
            "Hindi" => hindi.push(source),
            _ => rest.push(source),
        }
    }
    (english, hindi, rest)
}

/// Append one card — http(s) URLs only, deduped, with the `label ·
/// server` (or master `Auto (all variants) · server`) title and the
/// vidking hotlink headers — the port of `push`.
fn push(
    out: &mut Vec<NuvioStream>,
    seen: &mut HashSet<String>,
    source: &RawSource,
    server: &str,
    subtitles: &[NuvioSubtitle],
    master: bool,
) {
    if !source.url.starts_with("http") || !seen.insert(source.url.clone()) {
        return;
    }
    let label = if source.quality.is_empty() {
        "Auto".to_string()
    } else {
        source.quality.clone()
    };
    let title = if master {
        format!("Auto (all variants) · {server}")
    } else {
        format!("{label} · {server}")
    };
    let mut stream = NuvioStream::new(source.url.clone())
        .with_quality(label)
        .with_title(title)
        .with_name("Cineby")
        .with_header("User-Agent", UA)
        .with_header("Origin", VIDKING_ORIGIN)
        .with_header("Referer", VIDKING_REFERER);
    for subtitle in subtitles {
        stream = stream.with_subtitle(subtitle.clone());
    }
    out.push(stream);
}

/// The provider request URL — `URLSearchParams` order, plus the
/// provider's extra parameters.
fn provider_url(provider: &CinebyProvider, query: &ProviderQuery, seed: &str) -> Url {
    let mut url = Url::parse(&format!("{SPEEDRACELIGHT_API_BASE}/{}", provider.endpoint))
        .unwrap_or_else(|e| panic!("valid provider URL: {e}"));
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("title", &query.title);
        pairs.append_pair("mediaType", &query.media_type);
        pairs.append_pair(
            "year",
            &query.year.map(|year| year.to_string()).unwrap_or_default(),
        );
        pairs.append_pair("episodeId", &query.episode_id.to_string());
        pairs.append_pair("seasonId", &query.season_id.to_string());
        pairs.append_pair("tmdbId", &query.tmdb_id.to_string());
        pairs.append_pair("imdbId", query.imdb_id.as_deref().unwrap_or_default());
        pairs.append_pair("enc", "2");
        pairs.append_pair("seed", seed);
        pairs.append_pair("_t", &now_millis().to_string());
        if let Some((name, value)) = provider.extra {
            pairs.append_pair(name, value);
        }
    }
    url
}

/// `Date.now()` — the cache-busting `_t` parameter.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// Map the API's inline subtitles, skipping the dead playhq proxy —
/// the port of `mapSubtitles`.
fn map_subtitles(json: &Value) -> Vec<NuvioSubtitle> {
    let Some(subtitles) = json.get("subtitles").and_then(Value::as_array) else {
        return Vec::new();
    };
    subtitles
        .iter()
        .filter_map(|subtitle| {
            let url = subtitle.get("url")?.as_str()?;
            if DEAD_SUB_HOSTS.is_match(url).unwrap_or(false) {
                return None;
            }
            let lang = subtitle
                .get("lang")
                .or_else(|| subtitle.get("language"))
                .and_then(Value::as_str)
                .unwrap_or("en");
            Some(NuvioSubtitle {
                id: Some(lang.chars().take(8).collect()),
                url: Some(url.to_string()),
                lang: Some(lang.to_string()),
                ..NuvioSubtitle::default()
            })
        })
        .collect()
}

/// `/4k|2160/i → 2160; (\d{3,4}) → int; else 0` — the 4K-first sort
/// key.
fn quality_rank(quality: &str) -> u32 {
    let lower = quality.to_ascii_lowercase();
    if lower.contains("4k") || lower.contains("2160") {
        return 2160;
    }
    QUALITY_NUM
        .captures(&lower)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
        .unwrap_or(0)
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the display title.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
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

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear`.
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

/// `getImdbId` — the reference's own id when IMDb-keyed, the
/// pre-resolved media or `/external_ids` otherwise; failures are
/// best-effort `None` (upstream: `try { … } catch { }`).
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::{Format, MediaId};

    use super::*;

    // -- the scripted fetcher ------------------------------------------------

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

        /// How many requests hit `path`.
        fn hits(&self, path: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request.url.path() == path)
                .count()
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

    // -- fixtures ------------------------------------------------------------

    /// The seed + media pair every encrypted fixture below shares (the
    /// `speedracelight` ground-truth pair — payloads generated by
    /// re-encrypting with the bundle's own keystream via Node).
    const SEED: &str = "a1b2c3d4e5f6a7b8";
    const TMDB_ID: u64 = 123_456;

    /// The Yoru (cdn) sweep: 1080p + 2160p sources, one VTT subtitle,
    /// and the multi-variant master playlist.
    const CINEBY_CDN: &str = "dfS4GATTVMdIPIt14QZYTraUVVwxDX_-BD_Vx8QsdCkDqF9gwGT1pDumKfUB4qMPhkT3HxiD5d7Rb1YhvYUj_lI-P_B__TyuvQt6rELn027GyPFeuPo9SzkfNvg2B9Bqgjin_tSwUt3ZNpkO65L9NhNVEIg6EfvtkyefX9_mAVtCaQkxDClO8hBJWuNMzvPe2LumiJ2Oh_7Rsk3zWdWsYXmU-ckHaXatLaX-6d2_hBuoP85dkH7WU_jJXvcrZPM3zs5GfUjOcRT_5DUHkr0VGke8mkYY5hTGX-9n-2wvGtK0GkQDPMGE84NmRZH-QvwqHx380zcb9OvcqAqUNuiG2CmyO1L3KLb7-4ws0euO1SEEgN_eQ7atdW7DzETQNlKZ5gNkmE9D57XJCokU4ySoGaw7Y_P1WalQpg";
    /// The Breach (m4uhd) sweep: one `Auto HLS` master.
    const CINEBY_M4UHD: &str = "dfS4GATTVMdIPIt14QZYTraUVVwxDX_-BD_Vx8QsdCkYtUh0jWmjoC63KOBD5OIBxkG0DUKa5YfVawpk6M89vAglY7cr8SDoqV8x9Bm0nC3r1PFIuohTKStaW_U";
    /// The hdmovie sweep: the Vyse `English` / Fade `Hindi`
    /// language-as-quality tabs plus a master.
    const CINEBY_HDMOVIE: &str = "dfS4GATTVMdIPIt14QZYTraUVVwxDX_-BD_Vx8QsdCkMvl5wwGT1pDumKfUB4qMPhkT3HxiL5M_UYwp4o9AgpANyd7d2pjDxoUcholq042HNzexU8uJiVnIFc_p4WMYzyDmh4oWwX5rLI40Y_9iqLhFXCcNxCvXtzCOWApPhBRpObFRzUjMAuVlEGacbkrXDz_350vuK2u-AshyMBIK9YXGZ7IJOP2amOf36v9CRkhb0IshElnzTU6WOHM09NuogjI8TMg_bcBDj8HcblPBVFA";

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>, seeds: Arc<SeedStore>) -> Cineby {
        Cineby::new(Arc::new(TmdbClient::new("test-key", mock.clone())), seeds)
    }

    /// A context over the scripted fetcher.
    fn ctx_for(mock: &Arc<ScriptedFetcher>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = mock.as_ref();
        ResolveCtx {
            fetcher,
            media: None,
            source_id: None,
            referer: None,
        }
    }

    /// TMDB details + external ids for the fixture media.
    fn tmdb_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/movie/{TMDB_ID}"),
            200,
            r#"{"title":"Dune","release_date":"2021-10-22"}"#,
        )
        .page(
            format!("/3/movie/{TMDB_ID}/external_ids"),
            200,
            r#"{"imdb_id":"tt1160419"}"#,
        )
        .page("/seed", 200, format!(r#"{{"seed":"{SEED}"}}"#))
        .page("/cdn/sources-with-title", 200, CINEBY_CDN)
        .page("/m4uhd/sources-with-title", 200, CINEBY_M4UHD)
        .page("/hdmovie/sources-with-title", 200, CINEBY_HDMOVIE)
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let info = provider.info();
        assert_eq!(info.id, "cineby");
        assert_eq!(info.label, "Cineby");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://cineby.by/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn sweeps_all_live_servers_sorted_4k_first() -> Result<(), SourceError> {
        let mock = Arc::new(tmdb_pages(ScriptedFetcher::default()));
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // cdn: 2160p + 1080p + master, m4uhd: Auto HLS, hdmovie:
        // Vyse + Fade + master; meine/lamovie/superflix are unserved
        // (down/intermittent upstream).
        assert_eq!(streams.len(), 7);
        // 4K first.
        assert_eq!(streams[0].meta.resolution, Some(2160));
        assert_eq!(
            streams[0].url.as_str(),
            "https://yoru.example.com/hls/movie/2160/index.m3u8"
        );
        assert_eq!(streams[0].format, Format::Hls);
        // The vidking hotlink headers ride every card.
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://www.vidking.net/")
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some("https://www.vidking.net")
        );
        // Inline VTT subtitles from the cdn sweep.
        assert_eq!(streams[0].meta.subtitles.len(), 1);
        assert_eq!(
            streams[0].meta.subtitles[0].url.as_str(),
            "https://subs.example.com/cineby-en.vtt"
        );
        // The hdmovie split: exact-quality tabs carry language flags
        // via the title scan.
        let fade = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://fade.example.com/hls/hindi.m3u8")
            .unwrap_or_else(|| panic!("the Fade card exists"));
        assert!(fade.meta.languages.contains(&CountryCode::Hi));
        let vyse = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://vyse.example.com/hls/english.m3u8")
            .unwrap_or_else(|| panic!("the Vyse card exists"));
        assert!(vyse.meta.languages.contains(&CountryCode::En));
        // The master playlist card's title.
        assert!(
            streams[2]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Auto (all variants) · Yoru"))
        );
        // Every stream carries the provider identity.
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("cineby"))
        );
        assert_eq!(streams[0].ttl, TTL);
        // One shared seed served the whole sweep.
        assert_eq!(mock.hits("/seed"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_stale_seed_invalidates_and_retries_once() -> Result<(), SourceError> {
        // The first cdn body is garbage (bad seed → decrypt failure →
        // synthetic 401); the seed refetch serves the same seed again
        // and the second cdn body decrypts.
        let mock = Arc::new(tmdb_pages(ScriptedFetcher::default().page(
            "/cdn/sources-with-title",
            200,
            "not-a-encrypted-payload",
        )));
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // The retry recovers the full cdn sweep alongside the other
        // live tabs.
        assert_eq!(streams.len(), 7);
        assert!(
            streams.iter().any(|stream| stream.url.as_str()
                == "https://yoru.example.com/hls/movie/2160/index.m3u8")
        );
        assert_eq!(
            mock.hits("/seed"),
            2,
            "the seed was refetched after invalidation"
        );
        assert_eq!(mock.hits("/cdn/sources-with-title"), 2);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_sweep_retries_then_answers_not_found() {
        // TMDB resolves, every server tab is down.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/movie/{TMDB_ID}"),
                    200,
                    r#"{"title":"Dune","release_date":"2021-10-22"}"#,
                )
                .page("/seed", 200, format!(r#"{{"seed":"{SEED}"}}"#)),
        );
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        // The initial sweep plus the two empty retries.
        assert_eq!(mock.hits("/cdn/sources-with-title"), 3);
    }

    #[tokio::test]
    async fn a_down_speedracelight_fast_fails_without_fetching() {
        let mock = Arc::new(ScriptedFetcher::default());
        let seeds = Arc::new(SeedStore::new());
        seeds.mark_down();
        let provider = provider(&mock, seeds);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert!(
            mock.requests().is_empty(),
            "no TMDB or API fetches on the down mark"
        );
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(404)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/seed", 200, format!(r#"{{"seed":"{SEED}"}}"#))
                .page("/cdn/sources-with-title", 200, CINEBY_CDN),
        );
        let provider = provider(&mock, Arc::new(SeedStore::new()));
        let media = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: Some("tt1160419".to_string()),
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        };
        let ctx = ResolveCtx {
            fetcher: mock.as_ref() as &dyn Fetcher,
            media: Some(media),
            source_id: None,
            referer: None,
        };

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 3);
        assert!(mock.requests().iter().all(|request| {
            !request
                .url
                .host_str()
                .is_some_and(|host| host.contains("themoviedb"))
        }));
        Ok(())
    }

    #[test]
    fn ranks_qualities_4k_first() {
        assert_eq!(quality_rank("2160p"), 2160);
        assert_eq!(quality_rank("4K"), 2160);
        assert_eq!(quality_rank("1080p"), 1080);
        assert_eq!(quality_rank("Auto"), 0);
        assert_eq!(quality_rank(""), 0);
    }
}
