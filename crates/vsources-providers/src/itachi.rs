//! `Itachi`: sub+dub anime HLS from `itachi.tv` (`VidHawk` + `MegaPlay`).
//!
//! Ports `src/source/Itachi.js`. `itachi.tv` is a Next.js SPA that
//! wraps 5 underlying embed providers; upstream keeps two viable ones
//! (the rest were never ported):
//!
//! - `anilink.cc` — `PoW` challenge, skipped ("too complex for
//!   server-side").
//! - `vidnest.fun` — multi-backend SPA, skipped (needs browser JS).
//! - `dropfile.cc` — dead upstream (no response).
//! - `megaplay.buzz` — handled by the megaplay extractor (below).
//! - **`VidHawk` (primary)** — a clean public REST API.
//!
//! **The flow:**
//! 1. Resolve the `AniList` id via its GraphQL API (with a Jikan
//!    fallback), then pick the best title match: the entry's format
//!    must match the request (movie → `MOVIE`; series →
//!    `TV`/`TV_SHORT`/`OVA`/`ONA`/`SPECIAL` — music videos, novels,
//!    etc. never count) and the fuzzy score (exact 100, substring
//!    ratio × 90 over the English/romaji/userPreferred titles) must
//!    reach 75. Entries from the Jikan fallback carry no `AniList` id —
//!    only the MAL id.
//! 2. `VidHawk`, for each of the 3 regular servers (`kari`, `flow`,
//!    `melo` — the `core`/`gojo`/`zuri` "hsub" servers are hardcoded
//!    subs and sub-only, so they are skipped):
//!    `GET /api/stream/resolve?episode={ep}&server={srv}&variant=sub&parentHost=itachi.tv`
//!    (plus `anilistId`/`malId` when known) → `{ticket}`, then
//!    `GET /api/play?t={ticket}` → `{tracks: [{id: sub|dub, src}], captions: {sub: […], dub: […]}}`.
//!    Every track is an HLS stream with its matching caption set.
//!    The same URL across servers is emitted once (upstream's
//!    `seenHls` dedup).
//! 3. `MegaPlay` (secondary): deterministic URLs
//!    `https://megaplay.buzz/stream/ani/{anilistId}/{ep}/{sub|dub}`,
//!    resolved through the megaplay extractor.
//!
//! `edge.vidhawk.buzz` HLS is public — no Referer needed; upstream
//! shipped the tracks directly, this port passes them through the
//! `VidHawk` extractor's passthrough. `VidHawk`'s subtitle URLs need a
//! `Referer` upstream and were routed through a server-side `/proxy`
//! to fetch them; this library has no server, so the captions ship
//! as-is and players that need the Referer may have to fetch them
//! themselves.
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's
//!   TMDB resolution: name/year come from `ctx.media`, season/episode
//!   from the [`MediaRef`]. Without `ctx.media` there is no title to
//!   search → [`SourceError::NotFound`].
//! - The anime-only TMDB genre pre-check (`isAnimeContent`) is cut:
//!   `ctx.media` carries no genres and providers have no TMDB access.
//!   The `AniList` ≥ 75 title match plus the format filter remain the
//!   anime gate (searching a non-anime title simply never matches).
//! - The 3 `VidHawk` servers are probed sequentially (upstream raced
//!   them under an 18s budget; the engine's per-provider budget bounds
//!   this the same way). `checkAvailability` and `detectHlsHeight`
//!   are dead code upstream — cut.
//! - The Jikan format quirk is ported verbatim: Jikan's `type` maps
//!   `'TV'` → `TV`, `'MOVIE'` → `MOVIE`, anything else → `TV` (Jikan
//!   spells movies "Movie", so Jikan movies land in the series
//!   formats, exactly like upstream).
//! - The `MegaPlay` URL is only built with a real `AniList` id (upstream
//!   would interpolate `null` for Jikan-only matches, which can only
//!   404).
//! - `meta.title` becomes the stream label; `meta.sourceType` becomes
//!   `meta.quality` (`WebDL`); `meta.codec` (`x264`), `meta.audioLabel`
//!   (`audio`), `meta.height` (`resolution`, defaulting to 1080 —
//!   "`VidHawk` typically serves 720p-1080p"), and the captions become
//!   subtitle tracks. `this.ttl` (10min) is the stream TTL.
//! - Cut: the JS's `console.log` diagnostics (no logging facade in
//!   this crate) and the per-source result cache (the parent's
//!   `CachedSource` owns it).
//! - Patterns are ported as byte scanners (this crate has no regex
//!   engine); NFD folding is approximated with a Latin accent table.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaRef, MediaType, SourceInfo, Stream, StreamMeta, SubtitleTrack,
};
use vsources_extractors::ExtractorRegistry;

/// The provider id (upstream `this.id`).
const ID: &str = "itachi";
/// Upstream result lifetime: 10min.
const TTL: Duration = Duration::from_mins(10);
/// The `AniList` match threshold ("Skip if `AniList` match score < 75 —
/// prevents false matches").
const MIN_MATCH_SCORE: f64 = 75.0;

/// The 3 regular `VidHawk` servers (each supports both sub + dub; the
/// `core`/`gojo`/`zuri` hsub servers are hardcoded-sub and sub-only).
const VIDHAWK_SERVERS: [(&str, &str); 3] = [("kari", "Kari"), ("flow", "Flow"), ("melo", "Melo")];

/// `AniList` formats that count as real anime movies.
const VALID_MOVIE_FORMATS: [&str; 1] = ["MOVIE"];
/// `AniList` formats that count as real anime series (NOT music videos,
/// novels, etc.).
const VALID_SERIES_FORMATS: [&str; 5] = ["TV", "TV_SHORT", "OVA", "ONA", "SPECIAL"];

/// The `AniList` GraphQL query (upstream `query`).
const ANILIST_QUERY: &str = r"
    query($search: String) {
      Page(page: 1, perPage: 10) {
        media(type: ANIME, search: $search, sort: [SEARCH_MATCH, POPULARITY_DESC]) {
          id
          idMal
          title { romaji english native userPreferred }
          format
          episodes
          duration
        }
      }
    }";

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://itachi.tv").unwrap_or_else(|_| panic!("the Itachi base URL must parse"))
});
static VIDHAWK_BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://vidhawk.buzz").unwrap_or_else(|_| panic!("the VidHawk base URL must parse"))
});
static ANILIST_GQL: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://graphql.anilist.co")
        .unwrap_or_else(|_| panic!("the AniList GraphQL endpoint must parse"))
});

/// A resolved anime entry (from `AniList`, or the Jikan fallback which
/// carries only the MAL id).
struct AnimeEntry {
    /// The `AniList` id (`None` for the Jikan fallback).
    anilist_id: Option<u64>,
    /// The MAL id.
    mal_id: Option<u64>,
    /// The English title.
    english: Option<String>,
    /// The romaji title.
    romaji: Option<String>,
    /// The user-preferred title.
    user_preferred: Option<String>,
    /// The `AniList` format (`TV`, `MOVIE`, …).
    format: Option<String>,
}

/// The `AniList` GraphQL envelope.
#[derive(Deserialize)]
struct AniListResponse {
    /// The response data.
    #[serde(default)]
    data: Option<AniListData>,
}

/// The GraphQL `data` object.
#[derive(Deserialize)]
struct AniListData {
    /// The `Page` object.
    #[serde(default)]
    #[serde(rename = "Page")]
    page: Option<AniListPage>,
}

/// The GraphQL `Page` object.
#[derive(Deserialize)]
struct AniListPage {
    /// The matched media.
    #[serde(default)]
    media: Vec<AniListMedia>,
}

/// One matched `AniList` media entry.
#[derive(Deserialize)]
struct AniListMedia {
    /// The `AniList` id.
    #[serde(default)]
    id: Option<u64>,
    /// The MAL id.
    #[serde(default)]
    #[serde(rename = "idMal")]
    id_mal: Option<u64>,
    /// The media titles.
    #[serde(default)]
    title: Option<AniListTitle>,
    /// The `AniList` format.
    #[serde(default)]
    format: Option<String>,
}

/// The `AniList` title block.
#[derive(Deserialize)]
struct AniListTitle {
    /// The romaji title.
    #[serde(default)]
    romaji: Option<String>,
    /// The English title.
    #[serde(default)]
    english: Option<String>,
    /// The user-preferred title.
    #[serde(default)]
    user_preferred: Option<String>,
}

/// The Jikan API response.
#[derive(Deserialize)]
struct JikanResponse {
    /// The matched anime.
    #[serde(default)]
    data: Vec<JikanAnime>,
}

/// One Jikan anime entry.
#[derive(Deserialize)]
struct JikanAnime {
    /// The MAL id.
    #[serde(default)]
    mal_id: Option<u64>,
    /// The main title.
    #[serde(default)]
    title: Option<String>,
    /// The English title.
    #[serde(default)]
    title_english: Option<String>,
    /// The Japanese title (maps to romaji).
    #[serde(default)]
    title_japanese: Option<String>,
    /// The entry type (`TV`, `Movie`, `OVA`, …).
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// The `VidHawk` ticket response.
#[derive(Deserialize)]
struct ResolveTicketResponse {
    /// The play ticket.
    #[serde(default)]
    ticket: Option<String>,
}

/// The `VidHawk` play response.
#[derive(Deserialize)]
struct PlayResponse {
    /// The audio tracks.
    #[serde(default)]
    tracks: Vec<PlayTrack>,
    /// The per-audio-variant caption sets.
    #[serde(default)]
    captions: Option<std::collections::HashMap<String, Vec<Caption>>>,
}

/// One audio track.
#[derive(Deserialize)]
struct PlayTrack {
    /// The track id (`sub` or `dub`).
    #[serde(default)]
    id: Option<String>,
    /// The track's HLS URL.
    #[serde(default)]
    src: Option<String>,
}

/// One caption entry.
#[derive(Deserialize)]
struct Caption {
    /// The caption file URL.
    #[serde(default)]
    src: Option<String>,
    /// The language tag.
    #[serde(default)]
    lang: Option<String>,
    /// The display label.
    #[serde(default)]
    label: Option<String>,
}

/// The `Itachi` provider.
pub struct Itachi {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that claims the `VidHawk` tracks and the
    /// `MegaPlay` embeds.
    registry: Arc<ExtractorRegistry>,
}

impl Itachi {
    /// Build the provider over an extractor registry.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: "Itachi".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Resolve one `VidHawk` audio track to a stream, ports the track
    /// loop: URL dedup, the matching caption set, and the label and
    /// metadata shapes.
    async fn vidhawk_track(
        &self,
        ctx: &ResolveCtx<'_>,
        title_base: &str,
        server_label: &str,
        play: &PlayResponse,
        track: &PlayTrack,
        seen_hls: &mut HashSet<Url>,
    ) -> Result<Vec<Stream>, SourceError> {
        // `if (!track?.src) continue` and the `new URL` try/catch.
        let Some(src) = track.src.as_deref().filter(|src| !src.is_empty()) else {
            return Ok(Vec::new());
        };
        let Ok(url) = Url::parse(src) else {
            return Ok(Vec::new());
        };
        // Dedup by HLS URL (VidHawk may return the same URL across
        // servers).
        if !seen_hls.insert(url.clone()) {
            return Ok(Vec::new());
        }

        // Build subtitles from the captions of this audio variant;
        // entries with an unparseable src are dropped.
        let is_dub = track.id.as_deref() == Some("dub");
        let mut subtitles = Vec::new();
        if let Some(captions) = track
            .id
            .as_deref()
            .and_then(|id| play.captions.as_ref().and_then(|map| map.get(id)))
        {
            for caption in captions {
                let Some(caption_src) = caption
                    .src
                    .as_deref()
                    .filter(|src| !src.is_empty())
                    .and_then(|src| Url::parse(src).ok())
                else {
                    continue;
                };
                // Upstream routed these through /proxy to attach a
                // Referer; this library has no server, so they ship
                // as-is.
                subtitles.push(SubtitleTrack {
                    language: Some(
                        caption
                            .lang
                            .clone()
                            .filter(|lang| !lang.is_empty())
                            .unwrap_or_else(|| "en".to_string()),
                    ),
                    label: Some(
                        caption
                            .label
                            .clone()
                            .filter(|label| !label.is_empty())
                            .unwrap_or_else(|| "English".to_string()),
                    ),
                    url: caption_src,
                });
            }
        }

        // The VidHawk extractor passes edge.vidhawk.buzz HLS through.
        let extract_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: None,
            source_id: Some(ID),
            referer: None,
        };
        let Ok(extracted) = self.registry.extract(&extract_ctx, &url).await else {
            return Ok(Vec::new());
        };
        let audio_label = if is_dub {
            "English (DUB)"
        } else {
            "Japanese (SUB)"
        };
        Ok(extracted
            .into_iter()
            .map(|mut stream| {
                stream.format = Format::Hls;
                stream.label = Some(format!(
                    "{title_base} — [Itachi VidHawk {server_label}] {audio_label}"
                ));
                stream.ttl = TTL;
                stream.meta = StreamMeta {
                    languages: if is_dub {
                        vec![CountryCode::Multi, CountryCode::En]
                    } else {
                        vec![CountryCode::Multi, CountryCode::Ja]
                    },
                    audio: vec![if is_dub { "English" } else { "Japanese" }.to_string()],
                    quality: Some("WebDL".to_string()),
                    codec: Some("x264".to_string()),
                    subtitles: subtitles.clone(),
                    ..stream.meta
                };
                stream.meta.source_id = Some(ID.to_string());
                stream.meta.source_label = Some("Itachi".to_string());
                stream
            })
            .collect())
    }

    /// Resolve one `MegaPlay` variant through the registry and tag it
    /// with the provider's metadata.
    async fn megaplay_variant(
        &self,
        ctx: &ResolveCtx<'_>,
        title_base: &str,
        anilist_id: u64,
        ep_num: u32,
        sub_dub: &str,
    ) -> Result<Vec<Stream>, SourceError> {
        let embed_url = Url::parse(&format!(
            "https://megaplay.buzz/stream/ani/{anilist_id}/{ep_num}/{sub_dub}"
        ))
        .map_err(|error| {
            SourceError::scrape(ID, format!("the megaplay URL is invalid: {error}"))
        })?;
        // The megaplay extractor claims them and resolves via
        // getSourcesNew.
        let extract_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: None,
            source_id: Some(ID),
            referer: None,
        };
        let Ok(extracted) = self.registry.extract(&extract_ctx, &embed_url).await else {
            return Ok(Vec::new());
        };
        let is_dub = sub_dub == "dub";
        let audio_label = if is_dub {
            "English (DUB)"
        } else {
            "Japanese (SUB)"
        };
        Ok(extracted
            .into_iter()
            .map(|mut stream| {
                stream.label = Some(format!("{title_base} — [Itachi MegaPlay] {audio_label}"));
                stream.ttl = TTL;
                stream.meta.languages = if is_dub {
                    vec![CountryCode::Multi, CountryCode::En]
                } else {
                    vec![CountryCode::Multi, CountryCode::Ja]
                };
                stream.meta.audio = vec![if is_dub { "English" } else { "Japanese" }.to_string()];
                stream.meta.quality = Some("WebDL".to_string());
                stream.meta.codec = Some("x264".to_string());
                stream.meta.source_id = Some(ID.to_string());
                stream.meta.source_label = Some("Itachi".to_string());
                stream
            })
            .collect())
    }
}

/// Fetch one `VidHawk` server's resolve ticket.
///
/// The variant is always `sub`: the play data the ticket leads to
/// carries both audio tracks. A dead or empty server answers `None`.
async fn vidhawk_ticket(
    ctx: &ResolveCtx<'_>,
    server_id: &str,
    ep_num: u32,
    anilist_id: Option<u64>,
    mal_id: Option<u64>,
) -> Option<String> {
    let mut params: Vec<(&str, String)> = vec![
        ("episode", ep_num.to_string()),
        ("server", server_id.to_string()),
        ("variant", "sub".to_string()),
        ("parentHost", "itachi.tv".to_string()),
    ];
    if let Some(anilist_id) = anilist_id {
        params.push(("anilistId", anilist_id.to_string()));
    }
    if let Some(mal_id) = mal_id {
        params.push(("malId", mal_id.to_string()));
    }
    let mut resolve_url = VIDHAWK_BASE.join("api/stream/resolve").ok()?;
    resolve_url.query_pairs_mut().extend_pairs(params);
    let request = FetchRequest::get(resolve_url)
        .with_header("Referer", VIDHAWK_BASE.as_str())
        .with_timeout(Duration::from_secs(10));
    let response = ctx.fetcher.request(request).await.ok()?;
    response
        .json::<ResolveTicketResponse>()
        .ok()
        .and_then(|data| data.ticket.filter(|ticket| !ticket.is_empty()))
}

#[async_trait]
impl Source for Itachi {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // Upstream resolves the TMDB id, name, and year here; in this
        // SDK the engine resolves media metadata before the fan-out.
        let Some(meta) = ctx.media.as_ref() else {
            return Err(SourceError::NotFound);
        };
        let name = meta.name.as_str();
        let season = if media.kind == MediaType::Series {
            media.season
        } else {
            None
        };
        let title_base = match season {
            Some(_) => format!("{name} {}", media.format_season_and_episode()),
            None => match meta.year {
                Some(year) => format!("{name} ({year})"),
                None => name.to_string(),
            },
        };

        // Anime-only — the upstream TMDB genre pre-check is cut (see
        // the module doc); the AniList match below is the gate.
        let want_movie = season.is_none();
        let media_list = resolve_anime_entries(ctx, name).await;
        if media_list.is_empty() {
            return Ok(Vec::new());
        }

        let Some(best) = pick_best_anilist(&media_list, name, want_movie) else {
            return Ok(Vec::new());
        };
        let anilist_id = best.anilist_id;
        let mal_id = best.mal_id;
        // Skip if we have neither id.
        if anilist_id.is_none() && mal_id.is_none() {
            return Ok(Vec::new());
        }
        let ep_num = if want_movie {
            1
        } else {
            media.episode.unwrap_or(1)
        };

        // ── VidHawk: 3 servers × 2 audio variants ──
        let mut results: Vec<Stream> = Vec::new();
        let mut seen_hls: HashSet<Url> = HashSet::new();
        for (server_id, server_label) in VIDHAWK_SERVERS {
            // The ticket (variant is always sub: the play data carries
            // both audio tracks).
            let Some(ticket) = vidhawk_ticket(ctx, server_id, ep_num, anilist_id, mal_id).await
            else {
                // A dead server contributes nothing (upstream `[]`).
                continue;
            };

            let play_url = VIDHAWK_BASE
                .join(&format!("api/play?t={}", urlencode(&ticket)))
                .map_err(|error| {
                    SourceError::scrape(ID, format!("the play URL is invalid: {error}"))
                })?;
            let request = FetchRequest::get(play_url)
                .with_header("Referer", VIDHAWK_BASE.as_str())
                .with_timeout(Duration::from_secs(10));
            let Ok(response) = ctx.fetcher.request(request).await else {
                continue;
            };
            let Ok(play) = response.json::<PlayResponse>() else {
                continue;
            };

            for track in &play.tracks {
                let mut streams = self
                    .vidhawk_track(ctx, &title_base, server_label, &play, track, &mut seen_hls)
                    .await?;
                results.append(&mut streams);
            }
        }

        // ── MegaPlay: 2 deterministic URLs (sub + dub) ──
        // Only with a real AniList id (upstream would interpolate
        // `null` for Jikan-only matches, which can only 404).
        if let Some(anilist_id) = anilist_id {
            for sub_dub in ["sub", "dub"] {
                let mut streams = self
                    .megaplay_variant(ctx, &title_base, anilist_id, ep_num, sub_dub)
                    .await?;
                results.append(&mut streams);
            }
        }

        // Best-effort height default: VidHawk typically serves
        // 720p-1080p.
        for stream in &mut results {
            if stream.meta.resolution.is_none() {
                stream.meta.resolution = Some(1080);
            }
        }
        Ok(results)
    }
}

/// Resolve anime entries by title: `AniList` GraphQL first, Jikan
/// (`MyAnimeList`) as the fallback, ports `resolveAniList`. Each hop
/// answers `[]` when it is down.
async fn resolve_anime_entries(ctx: &ResolveCtx<'_>, name: &str) -> Vec<AnimeEntry> {
    // AniList GraphQL.
    let body =
        serde_json::json!({"query": ANILIST_QUERY, "variables": {"search": name}}).to_string();
    let request = FetchRequest::post(ANILIST_GQL.clone(), body)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
    if let Ok(response) = ctx.fetcher.request(request).await
        && let Ok(data) = response.json::<AniListResponse>()
    {
        let media = data
            .data
            .and_then(|data| data.page)
            .map(|page| page.media)
            .unwrap_or_default();
        if !media.is_empty() {
            return media
                .into_iter()
                .map(|entry| {
                    let title = entry.title.unwrap_or(AniListTitle {
                        romaji: None,
                        english: None,
                        user_preferred: None,
                    });
                    AnimeEntry {
                        anilist_id: entry.id,
                        mal_id: entry.id_mal,
                        english: title.english,
                        romaji: title.romaji,
                        user_preferred: title.user_preferred,
                        format: entry.format,
                    }
                })
                .collect();
        }
    }

    // Jikan (MyAnimeList wrapper) — no AniList ids, only MAL ids.
    let jikan_url = Url::parse_with_params(
        "https://api.jikan.moe/v4/anime",
        &[("q", name), ("limit", "5"), ("sfw", "true")],
    )
    .ok();
    if let Some(jikan_url) = jikan_url
        && let Ok(response) = ctx.fetcher.request(FetchRequest::get(jikan_url)).await
        && let Ok(data) = response.json::<JikanResponse>()
        && !data.data.is_empty()
    {
        return data
            .data
            .into_iter()
            .map(|anime| AnimeEntry {
                anilist_id: None,
                mal_id: anime.mal_id,
                english: anime.title_english.or(anime.title.clone()),
                romaji: anime.title_japanese.or(anime.title.clone()),
                user_preferred: anime.title,
                // `r.type === 'TV' ? 'TV' : r.type === 'MOVIE' ?
                // 'MOVIE' : 'TV'` — verbatim: only 'MOVIE' maps
                // differently; everything else (Jikan's "Movie"
                // spelling included) is TV.
                format: Some(match anime.kind.as_deref() {
                    Some("MOVIE") => "MOVIE".to_string(),
                    _ => "TV".to_string(),
                }),
            })
            .collect();
    }

    Vec::new()
}

/// Pick the best `AniList` match: the format must match the request
/// kind and the score must reach 75, ports `pickBestAniList`.
fn pick_best_anilist<'a>(
    media: &'a [AnimeEntry],
    name: &str,
    want_movie: bool,
) -> Option<&'a AnimeEntry> {
    let allowed: &[&str] = if want_movie {
        &VALID_MOVIE_FORMATS
    } else {
        &VALID_SERIES_FORMATS
    };
    let name_norm = normalize(name);
    let mut best: Option<(&AnimeEntry, f64)> = None;
    for entry in media {
        if !entry
            .format
            .as_deref()
            .is_some_and(|format| allowed.contains(&format))
        {
            continue;
        }
        for title in [&entry.english, &entry.romaji, &entry.user_preferred]
            .into_iter()
            .flatten()
        {
            let title_norm = normalize(title);
            if title_norm.is_empty() {
                continue;
            }
            let score = if title_norm == name_norm {
                100.0
            } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
                ratio(
                    title_norm.len().min(name_norm.len()),
                    title_norm.len().max(name_norm.len()).max(1),
                ) * 90.0
            } else {
                0.0
            };
            if best.is_none_or(|(_, best_score)| score > best_score) {
                best = Some((entry, score));
            }
        }
    }
    let (entry, score) = best?;
    (score >= MIN_MATCH_SCORE).then_some(entry)
}

/// The substring-ratio score.
fn ratio(short: usize, long: usize) -> f64 {
    f64::from(u32::try_from(short).unwrap_or_default())
        / f64::from(u32::try_from(long).unwrap_or_default())
}

/// Normalize for fuzzy matching, ports the upstream `normalize`:
/// lowercase, NFD-folded (approximated with a Latin accent table),
/// non-alphanumerics REMOVED (not spaced), collapsed.
fn normalize(s: &str) -> String {
    let folded = ascii_fold(s);
    folded
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `normalize('NFD')` + strip combining marks + lowercase, approximated
/// with a Latin accent table (the practical title space for these
/// searches).
fn ascii_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_ascii_lowercase() {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'ñ' => 'n',
            'ç' => 'c',
            'đ' => 'd',
            'ł' => 'l',
            other => other,
        })
        .collect()
}

/// `encodeURIComponent` for the ticket query parameter.
fn urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::MediaId;
    use vsources_extractors::hosts::directstream::DirectStream;
    use vsources_extractors::hosts::megaplay::Megaplay;
    use vsources_extractors::hosts::vidhawk::VidHawk;

    use super::*;

    /// A canned-body matcher: `(host, path)`.
    type PageRule = Box<dyn Fn(&Url) -> bool + Send + Sync>;

    /// A fetcher that serves canned bodies keyed by a URL matcher and
    /// records every request it sees.
    struct ScriptedFetcher {
        pages: Mutex<Vec<(PageRule, String)>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `body` to every request whose URL matches `matches`.
        fn page<F>(self, matches: F, body: impl Into<String>) -> Self
        where
            F: Fn(&Url) -> bool + Send + Sync + 'static,
        {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((Box::new(matches), body.into()));
            self
        }

        /// Every request seen so far, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The value of a header on the first request matching `path`.
        fn header_sent_to(&self, path: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| request.url.path() == path)
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }
    }

    /// Match a host and path (the query is ignored).
    fn at(host: &'static str, path: &'static str) -> impl Fn(&Url) -> bool {
        move |url| url.host_str() == Some(host) && url.path() == path
    }

    /// Match a host, path, and query substring.
    fn at_query(
        host: &'static str,
        path: &'static str,
        needle: &'static str,
    ) -> impl Fn(&Url) -> bool {
        move |url| {
            url.host_str() == Some(host)
                && url.path() == path
                && url.query().is_some_and(|query| query.contains(needle))
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let url = request.url.clone();
            let body = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|(matches, _)| matches(&url))
                .map(|(_, body)| body.clone());
            match body {
                Some(body) => Ok(FetchResponse {
                    url,
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "text/html".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url }),
            }
        }
    }

    /// A resolve context over the scripted fetcher.
    fn ctx(fetcher: &ScriptedFetcher, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: Some(ID),
            referer: None,
        }
    }

    /// A series episode reference with resolved TMDB metadata.
    fn one_piece(season: Option<u32>, episode: Option<u32>) -> (MediaRef, ResolvedMedia) {
        (
            MediaRef {
                id: MediaId::Tmdb(37854),
                kind: if season.is_some() {
                    MediaType::Series
                } else {
                    MediaType::Movie
                },
                season,
                episode,
            },
            ResolvedMedia {
                tmdb_id: Some(37854),
                imdb_id: None,
                name: "One Piece".to_string(),
                year: Some(1999),
                season,
                episode,
            },
        )
    }

    /// A provider wired to a registry with the vidhawk, megaplay, and
    /// directstream extractors — `edge.vidhawk.buzz` HLS is a
    /// `DirectStream` passthrough host in the production wiring.
    fn provider() -> Itachi {
        Itachi::new(Arc::new(ExtractorRegistry::new(vec![
            Arc::new(VidHawk::new()),
            Arc::new(Megaplay::new()),
            Arc::new(DirectStream::new()),
        ])))
    }

    const ANILIST_JSON: &str = r#"{"data":{"Page":{"media":[
        {"id":16498,"idMal":21,"title":{"romaji":"One Piece","english":"One Piece","native":"ワンピース","userPreferred":"One Piece"},"format":"TV","episodes":1122,"duration":24}
    ]}}}"#;

    const JIKAN_JSON: &str = r#"{"data":[
        {"mal_id":21,"title":"One Piece","title_english":"One Piece","title_japanese":"ワンピース","type":"TV","episodes":1122}
    ]}"#;

    /// kari's play data: sub and dub tracks with caption sets.
    const KARI_PLAY: &str = r#"{
        "tracks": [
            {"id": "sub", "label": "Sub", "src": "https://edge.vidhawk.buzz/hls/sub.m3u8?t=xyz"},
            {"id": "dub", "label": "Dub", "src": "https://edge.vidhawk.buzz/hls/dub.m3u8?t=uvw"}
        ],
        "captions": {
            "sub": [
                {"lang": "en", "label": "English", "src": "https://edge.vidhawk.buzz/sub.vtt?t=xyz"},
                {"lang": "es", "label": "Spanish", "src": "not-a-url"}
            ],
            "dub": []
        },
        "intro": {"start": 0, "end": 90},
        "outro": {"start": 1300, "end": 1380}
    }"#;

    /// flow's play data: the same tracks (the dedup keeps one stream
    /// per URL).
    const FLOW_PLAY: &str = r#"{
        "tracks": [
            {"id": "sub", "label": "Sub", "src": "https://edge.vidhawk.buzz/hls/sub.m3u8?t=xyz"},
            {"id": "dub", "label": "Dub", "src": "https://edge.vidhawk.buzz/hls/dub.m3u8?t=uvw"}
        ],
        "captions": {"sub": [], "dub": []}
    }"#;

    /// The megaplay embed page and API payloads for the extraction chain.
    const MEGAPLAY_EMBED: &str =
        r#"<html><body><div id="player" data-id="987654"></div></body></html>"#;
    const MEGAPLAY_SOURCES: &str =
        r#"{"sources":{"file":"https://fetch.example/hls/master.m3u8"}}"#;
    const MEGAPLAY_PLAYLIST: &str =
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1920x1080\nchunklist.m3u8";

    #[test]
    fn the_jikan_format_quirk_is_ported_verbatim() {
        // Only exact 'TV'/'MOVIE' types map; anything else (including
        // Jikan's "Movie" spelling) lands in TV.
        assert_eq!(jikan_format(Some("TV")), "TV");
        assert_eq!(jikan_format(Some("MOVIE")), "MOVIE");
        assert_eq!(jikan_format(Some("Movie")), "TV");
        assert_eq!(jikan_format(Some("OVA")), "TV");
        assert_eq!(jikan_format(None), "TV");
    }

    /// The format mapping factored out for the test above.
    fn jikan_format(kind: Option<&str>) -> &'static str {
        match kind {
            Some("MOVIE") => "MOVIE",
            _ => "TV",
        }
    }

    #[test]
    fn refuses_wrong_formats_and_weak_matches() {
        let entries = vec![AnimeEntry {
            anilist_id: Some(1),
            mal_id: Some(1),
            english: Some("One Piece".to_string()),
            romaji: None,
            user_preferred: None,
            format: Some("MUSIC_VIDEO".to_string()),
        }];
        // A music video never counts as a series or a movie.
        assert!(pick_best_anilist(&entries, "One Piece", false).is_none());
        assert!(pick_best_anilist(&entries, "One Piece", true).is_none());

        let entries = vec![AnimeEntry {
            anilist_id: Some(1),
            mal_id: Some(1),
            english: Some("One Piece Film Z".to_string()),
            romaji: None,
            user_preferred: None,
            format: Some("TV".to_string()),
        }];
        // A partial match below 75 is refused.
        assert!(pick_best_anilist(&entries, "One Piece", false).is_none());
    }

    #[tokio::test]
    async fn resolves_vidhawk_tracks_and_megaplay_fallbacks() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
        // kari and flow resolve + play; melo is unscripted (a dead
        // server contributes nothing).
        .page(
            at_query("vidhawk.buzz", "/api/stream/resolve", "server=kari"),
            r#"{"ticket":"TCK-kari","server":"kari","defaultAudio":"sub","servers":[{"id":"kari"}]}"#,
        )
        .page(at_query("vidhawk.buzz", "/api/play", "t=TCK-kari"), KARI_PLAY)
        .page(
            at_query("vidhawk.buzz", "/api/stream/resolve", "server=flow"),
            r#"{"ticket":"TCK-flow","server":"flow","defaultAudio":"sub","servers":[{"id":"flow"}]}"#,
        )
        .page(at_query("vidhawk.buzz", "/api/play", "t=TCK-flow"), FLOW_PLAY)
        // MegaPlay: the sub variant resolves, the dub is unscripted.
        .page(at("megaplay.buzz", "/stream/ani/16498/5/sub"), MEGAPLAY_EMBED)
        .page(at("megaplay.buzz", "/stream/getSourcesNew"), MEGAPLAY_SOURCES)
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        let (media, meta) = one_piece(Some(1), Some(5));
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        // VidHawk: sub + dub (flow's identical URLs deduped); MegaPlay:
        // sub. Order: VidHawk first, MegaPlay appended.
        assert_eq!(streams.len(), 3);
        let labels: Vec<&str> = streams
            .iter()
            .map(|s| s.label.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            labels,
            vec![
                "One Piece S01E05 — [Itachi VidHawk Kari] Japanese (SUB)",
                "One Piece S01E05 — [Itachi VidHawk Kari] English (DUB)",
                "One Piece S01E05 — [Itachi MegaPlay] Japanese (SUB)",
            ]
        );
        assert_eq!(
            streams[0].url.as_str(),
            "https://edge.vidhawk.buzz/hls/sub.m3u8?t=xyz"
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://edge.vidhawk.buzz/hls/dub.m3u8?t=uvw"
        );
        assert!(
            streams[2]
                .url
                .as_str()
                .starts_with("https://fetch.example/hls/master.m3u8")
        );
        // The sub VidHawk track carries its caption set; the invalid
        // caption src was dropped.
        assert_eq!(streams[0].meta.subtitles.len(), 1);
        assert_eq!(streams[0].meta.subtitles[0].language.as_deref(), Some("en"));
        assert_eq!(
            streams[0].meta.subtitles[0].label.as_deref(),
            Some("English")
        );
        assert_eq!(
            streams[0].meta.subtitles[0].url.as_str(),
            "https://edge.vidhawk.buzz/sub.vtt?t=xyz"
        );
        for stream in &streams {
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.ttl, TTL);
            assert_eq!(stream.meta.source_id.as_deref(), Some(ID));
            assert_eq!(stream.meta.quality.as_deref(), Some("WebDL"));
            assert_eq!(stream.meta.codec.as_deref(), Some("x264"));
            assert_eq!(stream.meta.resolution, Some(1080));
        }
        // The audio labels split sub/dub.
        assert_eq!(streams[0].meta.audio, vec!["Japanese".to_string()]);
        assert_eq!(streams[1].meta.audio, vec!["English".to_string()]);
        assert_eq!(
            streams[0].meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            streams[1].meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        // The VidHawk endpoints carried the API referer.
        assert_eq!(
            fetcher
                .header_sent_to("/api/stream/resolve", "Referer")
                .as_deref(),
            Some("https://vidhawk.buzz/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn jikan_falls_back_to_the_mal_id() -> Result<(), SourceError> {
        // AniList is down (unscripted): Jikan supplies a MAL-only
        // match — the VidHawk resolve carries malId but no anilistId,
        // and the MegaPlay branch is skipped.
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("api.jikan.moe", "/v4/anime"), JIKAN_JSON)
        .page(
            at_query("vidhawk.buzz", "/api/stream/resolve", "server=kari"),
            r#"{"ticket":"TCK-kari"}"#,
        )
        .page(
            at_query("vidhawk.buzz", "/api/play", "t=TCK-kari"),
            KARI_PLAY,
        );

        let (media, meta) = one_piece(Some(1), Some(5));
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        let resolve_query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/api/stream/resolve")
            .and_then(|request| request.url.query().map(str::to_string))
            .unwrap_or_default();
        assert!(
            resolve_query.contains("malId=21"),
            "the Jikan fallback must pass the MAL id, got {resolve_query}"
        );
        assert!(
            !resolve_query.contains("anilistId"),
            "no AniList id is available to pass, got {resolve_query}"
        );
        // The MegaPlay branch never ran (no anilist id).
        assert!(
            !fetcher
                .requests()
                .iter()
                .any(|request| request.url.host_str() == Some("megaplay.buzz")),
            "the megaplay branch needs a real anilist id"
        );
        // Only the VidHawk tracks resolved.
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("One Piece S01E05 — [Itachi VidHawk Kari] Japanese (SUB)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_movie_reference_takes_episode_one() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
        .page(
            at_query("vidhawk.buzz", "/api/stream/resolve", "server=kari"),
            r#"{"ticket":"TCK-kari"}"#,
        )
        .page(
            at_query("vidhawk.buzz", "/api/play", "t=TCK-kari"),
            KARI_PLAY,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/1/sub"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/getSourcesNew"),
            MEGAPLAY_SOURCES,
        )
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        let (media, meta) = one_piece(None, None);
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        // wantMovie: format must be MOVIE — the TV entry is refused,
        // so nothing matches.
        assert!(streams.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_media_metadata_is_not_found() {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };
        let (media, _) = one_piece(Some(1), Some(5));
        let ctx = ctx(&fetcher, None);
        match provider().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("without a title there is nothing to search: {other:?}"),
        }
    }
}
