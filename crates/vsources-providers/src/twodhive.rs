//! `2Dhive`: MAL-id-keyed anime embeds from `2dhive.com`.
//!
//! Ports `src/source/TwoDhive.js` — a clean JSON API, no scraping
//! (the site is a MAL-ID-keyed anime-only archive, ~10,648 titles):
//!
//! 1. `GET https://2dhive.com/api/search?q={title}` →
//!    `{results: [{id (MAL ID), title, englishTitle, …}]}`. The best
//!    match must score ≥ 60 (exact 100, substring ratio × 90 over the
//!    romaji and English titles); each query variant (raw, NFD-folded,
//!    punctuation-stripped) is tried in turn.
//! 2. The embed URL is built directly — no episode page needed:
//!    `https://megaplay.buzz/stream/mal/{malId}/{ep}/{sub|dub}`, and
//!    the megaplay extractor resolves it to a direct m3u8 via
//!    `getSourcesNew`.
//!
//! Both SUB and DUB URLs are emitted; DUB may 410 for titles without
//! dubs, which the megaplay extractor handles gracefully (the failed
//! variant is dropped, not fatal). The site's `BabaStream` secondary
//! server (`babastream.top`) is CF-Turnstile-protected and was never
//! viable server-side — not ported, like upstream.
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's
//!   TMDB resolution: name/year come from `ctx.media`, season/episode
//!   from the [`MediaRef`]. Without `ctx.media` there is no title to
//!   search → [`SourceError::NotFound`].
//! - The JS's `got-scraping` transport and Chrome `User-Agent` string
//!   collapse onto the shared fetcher (browser TLS impersonation and
//!   the browser-like headers live there).
//! - The source emits no `format` (the JS result has none) — the
//!   megaplay extractor's HLS stands. `meta.title` becomes the stream
//!   label, `meta.countryCodes` become `meta.languages`. The JS's 12h
//!   result TTL was capped at 15min by the resolver's cache, so
//!   streams carry 15min.
//! - Cut: the JS's per-source result cache (the parent's
//!   `CachedSource` owns it). Patterns are ported as byte scanners
//!   (this crate has no regex engine); NFD folding is approximated
//!   with a Latin accent table.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id (upstream `this.id`).
const ID: &str = "2dhive";
/// Upstream effective result lifetime: `this.ttl` is the 12h default,
/// which the resolver's cache capped at 15min.
const TTL: Duration = Duration::from_mins(15);
/// Best-match threshold (upstream `bestScore >= 60`).
const MIN_MATCH_SCORE: f64 = 60.0;

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://2dhive.com").unwrap_or_else(|_| panic!("the 2Dhive base URL must parse"))
});

/// The search API response.
#[derive(Deserialize)]
struct SearchResponse {
    /// The matched anime.
    #[serde(default)]
    results: Vec<SearchResult>,
}

/// One search result row.
#[derive(Deserialize)]
struct SearchResult {
    /// The MAL id — rows without one cannot build an embed URL.
    #[serde(default)]
    id: Option<u64>,
    /// The romaji title.
    #[serde(default)]
    title: Option<String>,
    /// The English title.
    #[serde(default)]
    #[serde(rename = "englishTitle")]
    english_title: Option<String>,
}

/// The `2Dhive` provider.
pub struct TwoDhive {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that resolves the megaplay embeds.
    registry: Arc<ExtractorRegistry>,
}

impl TwoDhive {
    /// Build the provider over an extractor registry.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: "2Dhive".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Search for the anime and return the MAL id of the best match,
    /// ports `findMalId`: each query variant is tried until one
    /// scores ≥ 60; a failed request or bad JSON answers `None`.
    async fn find_mal_id(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
    ) -> Result<Option<u64>, SourceError> {
        let name_norm = normalize(name);
        for query in candidate_queries(name) {
            let url =
                Url::parse_with_params("https://2dhive.com/api/search", &[("q", query.as_str())])
                    .map_err(|error| {
                    SourceError::scrape(ID, format!("the search URL is invalid: {error}"))
                })?;
            let request = FetchRequest::get(url)
                .with_header("Accept", "application/json")
                .with_timeout(Duration::from_secs(15));
            let Ok(response) = ctx.fetcher.request(request).await else {
                // Upstream `apiGet → null` on a failed request.
                continue;
            };
            let Ok(data) = response.json::<SearchResponse>() else {
                continue;
            };

            let mut best: Option<(u64, f64)> = None;
            for result in data.results {
                let Some(id) = result.id else {
                    continue;
                };
                let mut item_best: f64 = 0.0;
                for title in [result.title, result.english_title].into_iter().flatten() {
                    let title_norm = normalize(&title);
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
                    item_best = item_best.max(score);
                }
                if best.is_none_or(|(_, best_score)| item_best > best_score) {
                    best = Some((id, item_best));
                }
            }
            if let Some((id, score)) = best
                && score >= MIN_MATCH_SCORE
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl Source for TwoDhive {
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
        let title = match season {
            Some(_) => format!("{name} {}", media.format_season_and_episode()),
            None => match meta.year {
                Some(year) => format!("{name} ({year})"),
                None => name.to_string(),
            },
        };

        // Step 1: search for the anime's MAL id.
        let Some(mal_id) = self.find_mal_id(ctx, name).await? else {
            return Ok(Vec::new());
        };

        // Step 2: build the embed URLs for both sub and dub — the
        // megaplay extractor resolves them to direct m3u8s.
        let ep_num = season.map_or(1, |_| media.episode.unwrap_or(1));
        let mut streams = Vec::new();
        for sub_dub in ["sub", "dub"] {
            let embed_url = Url::parse(&format!(
                "https://megaplay.buzz/stream/mal/{mal_id}/{ep_num}/{sub_dub}"
            ))
            .map_err(|error| {
                SourceError::scrape(ID, format!("the embed URL is invalid: {error}"))
            })?;
            let extract_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: None,
                source_id: Some(ID),
                referer: None,
            };
            // DUB may 410 for titles without dubs: the extractor maps
            // it to a miss and the variant is dropped.
            let Ok(extracted) = self.registry.extract(&extract_ctx, &embed_url).await else {
                continue;
            };
            let is_dub = sub_dub == "dub";
            for mut stream in extracted {
                stream.label = Some(format!("{title} ({})", if is_dub { "Dub" } else { "Sub" }));
                stream.ttl = TTL;
                stream.meta.languages = if is_dub {
                    vec![CountryCode::Multi, CountryCode::En]
                } else {
                    vec![CountryCode::Multi, CountryCode::Ja]
                };
                stream.meta.source_id = Some(ID.to_string());
                stream.meta.source_label = Some("2Dhive".to_string());
                streams.push(stream);
            }
        }
        Ok(streams)
    }
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

/// The three upstream query variants (raw, NFD-folded,
/// punctuation-stripped), deduped.
fn candidate_queries(name: &str) -> Vec<String> {
    let folded = ascii_fold(name);
    let alnum = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut queries: Vec<String> = Vec::new();
    for query in [name.to_string(), folded, alnum] {
        if !query.is_empty() && !queries.contains(&query) {
            queries.push(query);
        }
    }
    queries
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::{Format, MediaId};
    use vsources_extractors::hosts::megaplay::Megaplay;

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
    }

    /// Match a host and path (the query is ignored).
    fn at(host: &'static str, path: &'static str) -> impl Fn(&Url) -> bool {
        move |url| url.host_str() == Some(host) && url.path() == path
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
    fn one_piece() -> (MediaRef, ResolvedMedia) {
        (
            MediaRef {
                id: MediaId::Tmdb(37854),
                kind: MediaType::Series,
                season: Some(1),
                episode: Some(5),
            },
            ResolvedMedia {
                tmdb_id: Some(37854),
                imdb_id: None,
                name: "One Piece".to_string(),
                year: Some(1999),
                season: Some(1),
                episode: Some(5),
            },
        )
    }

    /// A provider wired to a registry with the megaplay extractor.
    fn provider() -> TwoDhive {
        TwoDhive::new(Arc::new(ExtractorRegistry::new(vec![Arc::new(
            Megaplay::new(),
        )])))
    }

    const SEARCH_JSON: &str = r#"{"results":[
        {"id": 21, "title": "One Piece", "englishTitle": "One Piece", "type": "TV", "episodes": 1122, "year": 1999},
        {"id": 999, "title": "Straw Hat Shorts", "englishTitle": null}
    ]}"#;

    /// The megaplay embed page and API payloads for the extraction chain.
    const MEGAPLAY_EMBED: &str =
        r#"<html><body><div id="player" data-id="987654"></div></body></html>"#;
    const MEGAPLAY_SOURCES: &str = r#"{"sources":{"file":"https://fetch.example/hls/master.m3u8"},"tracks":[{"file":"https://fetch.example/sub.vtt","label":"english","kind":"captions"}]}"#;
    const MEGAPLAY_PLAYLIST: &str =
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1920x1080\nchunklist.m3u8";

    #[tokio::test]
    async fn builds_sub_and_dub_megaplay_embeds() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("2dhive.com", "/api/search"), SEARCH_JSON)
        // Only the sub embed resolves; the dub variant is unscripted
        // (upstream: DUB 410s for titles without dubs and is dropped).
        .page(at("megaplay.buzz", "/stream/mal/21/5/sub"), MEGAPLAY_EMBED)
        .page(at("megaplay.buzz", "/stream/getSourcesNew"), MEGAPLAY_SOURCES)
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        assert_eq!(streams.len(), 1, "sub resolved, dub 410'd");
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("One Piece S01E05 (Sub)"));
        assert_eq!(
            stream.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(stream.meta.source_id.as_deref(), Some(ID));
        assert_eq!(stream.meta.source_label.as_deref(), Some("2Dhive"));
        // The embed URL was built from the searched MAL id.
        assert!(
            fetcher
                .requests()
                .iter()
                .any(|request| request.url.path() == "/stream/mal/21/5/sub"),
            "the mal-keyed embed must be probed"
        );
        // The extractor's result stands (format, referer, subtitles,
        // resolution).
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.subtitles.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_weak_match_is_refused() -> Result<(), SourceError> {
        // No result scores >= 60.
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(
            at("2dhive.com", "/api/search"),
            r#"{"results":[{"id":999,"title":"Straw Hat Shorts","englishTitle":null}]}"#,
        );

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(streams.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_search_api_is_an_empty_answer() -> Result<(), SourceError> {
        // Nothing scripted: the search API 404s.
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(streams.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_media_metadata_is_not_found() {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };
        let (media, _) = one_piece();
        let ctx = ctx(&fetcher, None);
        match provider().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("without a title there is nothing to search: {other:?}"),
        }
    }

    #[test]
    fn normalizes_titles_for_matching() {
        assert_eq!(normalize("Journey’s End"), "journeys end");
        assert_eq!(normalize("One-Piece!"), "onepiece");
    }
}
