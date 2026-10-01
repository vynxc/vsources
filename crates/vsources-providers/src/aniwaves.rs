//! `AniWaves` (`aniwaves.ru`): current AJAX catalog and `EchoVideo` dub streams.
//!
//! Protocol checked against the deployed site and aryaniiil/anime-api's
//! `aniwaves.py` (594d12ec). Matching validates title, type and release year;
//! later seasons require an explicit season title instead of guessing an offset.
//! English-only resolution skips SUB servers and returns the first direct DUB
//! extraction. No unrelated media fallback or provider-local body downloads.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use moka::future::Cache;
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::hosts::{echovideo::EchoVideo, megaplay::Megaplay};

const BASE: &str = "https://aniwaves.ru";
const TIMEOUT: Duration = Duration::from_secs(8);
static TITLES: LazyLock<Selector> = LazyLock::new(|| selector("a.name.d-title"));
static EPISODES: LazyLock<Selector> = LazyLock::new(|| selector("a[data-num][data-ids]"));
static GROUPS: LazyLock<Selector> = LazyLock::new(|| selector("div[data-type]"));
static SERVERS: LazyLock<Selector> = LazyLock::new(|| selector("li[data-link-id]"));
static DETAILS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Type|Date aired|Premiered):\s*([^\n]+)")
        .unwrap_or_else(|e| panic!("detail pattern: {e}"))
});
static YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:19|20)\d{2}\b").unwrap_or_else(|e| panic!("year pattern: {e}"))
});
static SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:season\s*(\d+)|(\d+)(?:st|nd|rd|th)\s+season)\b")
        .unwrap_or_else(|e| panic!("season pattern: {e}"))
});

fn selector(raw: &str) -> Selector {
    Selector::parse(raw).unwrap_or_else(|e| panic!("selector: {e}"))
}

#[derive(Clone)]
struct Candidate {
    slug: String,
    title: String,
    japanese: String,
    id: u64,
}

struct Server {
    id: String,
    label: String,
    dub: bool,
}

/// Native `AniWaves` provider with a bounded five-minute catalog identity cache.
pub struct AniWaves {
    info: SourceInfo,
    identities: Cache<(String, Option<u16>, u32, MediaType), Candidate>,
}

impl Default for AniWaves {
    fn default() -> Self {
        Self::new()
    }
}

impl AniWaves {
    /// Construct without network I/O or credentials.
    #[must_use]
    pub fn new() -> Self {
        Self {
            info: SourceInfo {
                id: "aniwaves".into(),
                label: "AniWaves".into(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Ja, CountryCode::En],
                base_url: Url::parse(BASE).ok(),
                priority: 0,
                domain_key: None,
            },
            identities: Cache::builder()
                .max_capacity(128)
                .time_to_live(Duration::from_mins(5))
                .build(),
        }
    }

    async fn identity(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Candidate, SourceError> {
        let meta = ctx.media.as_ref().ok_or(SourceError::NotFound)?;
        let season = media.season.unwrap_or(1);
        let key = (meta.name.clone(), meta.year, season, media.kind);
        self.identities
            .try_get_with(key, async {
                let titles = title_queries(&meta.name, season);
                let search_jobs: Vec<BoxFuture<'_, Option<String>>> = titles
                    .iter()
                    .map(|title| {
                        async move {
                            let mut url = site_url("/filter")?;
                            url.query_pairs_mut().append_pair("keyword", title);
                            get(ctx, url, BASE).await
                        }
                        .boxed()
                    })
                    .collect();
                let pages = stream::iter(search_jobs)
                    .buffer_unordered(3)
                    .collect::<Vec<_>>()
                    .await;
                let mut candidates = Vec::new();
                let mut seen = HashSet::new();
                for page in pages.into_iter().flatten() {
                    for candidate in parse_candidates(&page) {
                        if !season_matches(&candidate, season) {
                            continue;
                        }
                        if seen.insert(candidate.id) {
                            let score = title_score(&titles, &candidate);
                            if score >= 0.68 {
                                candidates.push((candidate, score));
                            }
                        }
                    }
                }
                candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
                candidates.truncate(4);
                let detail_jobs: Vec<BoxFuture<'_, Option<(Candidate, f64)>>> = candidates
                    .into_iter()
                    .map(|(candidate, score)| {
                        async move {
                            let page =
                                get(ctx, site_url(&format!("/watch/{}", candidate.slug))?, BASE)
                                    .await?;
                            // TMDB's year is the series' first season year; later seasons
                            // must match an explicit season title, not that original year.
                            let year = if season == 1 { meta.year } else { None };
                            if !valid_detail(&page, media.kind, year) {
                                return None;
                            }
                            Some((candidate, score))
                        }
                        .boxed()
                    })
                    .collect();
                let details = stream::iter(detail_jobs)
                    .buffer_unordered(3)
                    .collect::<Vec<_>>()
                    .await;
                confident_match(details.into_iter().flatten().collect())
                    .ok_or(SourceError::NotFound)
            })
            .await
            .map_err(|error| error.as_ref().clone())
    }

    async fn streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        english_only: bool,
    ) -> Result<Vec<Stream>, SourceError> {
        let candidate = self.identity(ctx, media).await?;
        let referer = format!("{BASE}/watch/{}", candidate.slug);
        let mut url = site_url(&format!("/ajax/episode/list/{}", candidate.id))
            .ok_or(SourceError::NotFound)?;
        url.query_pairs_mut().append_pair("vrf", "");
        let result = ajax(ctx, url, &referer)
            .await
            .ok_or(SourceError::NotFound)?;
        let episode = if media.kind == MediaType::Movie {
            1
        } else {
            media.episode.unwrap_or(1)
        };
        let source_number = episode_slug(result.as_str().unwrap_or_default(), episode)
            .ok_or(SourceError::NotFound)?;
        let referer = format!("{referer}/ep-{source_number}");
        let mut url = site_url("/ajax/server/list").ok_or(SourceError::NotFound)?;
        url.query_pairs_mut()
            .append_pair("servers", &candidate.id.to_string())
            .append_pair("eps", &source_number);
        let result = ajax(ctx, url, &referer)
            .await
            .ok_or(SourceError::NotFound)?;
        let servers: Vec<_> = parse_servers(result.as_str().unwrap_or_default())
            .into_iter()
            .filter(|server| !english_only || server.dub)
            .collect();
        let jobs: Vec<BoxFuture<'_, Vec<Stream>>> = servers
            .into_iter()
            .map(|server| {
                let referer = referer.clone();
                async move { extract_server(ctx, server, &referer).await }.boxed()
            })
            .collect();
        let mut pending = stream::iter(jobs).buffer_unordered(3);
        let mut streams = Vec::new();
        let mut seen = HashSet::new();
        while let Some(cards) = pending.next().await {
            for stream in cards {
                if seen.insert((stream.url.clone(), stream.meta.dubbed)) {
                    streams.push(stream);
                }
            }
            if english_only && !streams.is_empty() {
                break;
            }
        }
        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

#[async_trait]
impl Source for AniWaves {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        self.streams(ctx, media, false).await
    }

    async fn resolve_english_dub(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        self.streams(ctx, media, true).await
    }
}

fn site_url(path: &str) -> Option<Url> {
    Url::parse(BASE).ok()?.join(path).ok()
}

async fn get(ctx: &ResolveCtx<'_>, url: Url, referer: &str) -> Option<String> {
    let response = ctx
        .fetcher
        .request(
            FetchRequest::get(url)
                .with_header("Referer", referer)
                .with_timeout(TIMEOUT),
        )
        .await
        .ok()?;
    response.is_success().then_some(response.body)
}

async fn ajax(ctx: &ResolveCtx<'_>, url: Url, referer: &str) -> Option<Value> {
    let response = ctx
        .fetcher
        .request(
            FetchRequest::get(url)
                .with_header("Referer", referer)
                .with_header("X-Requested-With", "XMLHttpRequest")
                .with_timeout(TIMEOUT),
        )
        .await
        .ok()?;
    if !response.is_success() {
        return None;
    }
    let json: Value = response.json().ok()?;
    if json
        .get("status")
        .and_then(|s| s.as_u64().or_else(|| s.as_str()?.parse().ok()))
        != Some(200)
    {
        return None;
    }
    json.get("result").cloned()
}

fn parse_candidates(page: &str) -> Vec<Candidate> {
    Html::parse_document(page)
        .select(&TITLES)
        .filter_map(|a| {
            let slug = a.value().attr("href")?.strip_prefix("/watch/")?;
            if slug.is_empty() || slug.contains(['/', '?', '#']) {
                return None;
            }
            let id = slug.rsplit_once('-')?.1.parse().ok()?;
            Some(Candidate {
                slug: slug.into(),
                id,
                title: a.text().collect::<String>().trim().into(),
                japanese: a.value().attr("data-jp").unwrap_or_default().into(),
            })
        })
        .collect()
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric())
        .collect()
}

fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let grams = |s: &str| -> HashSet<_> {
        s.chars()
            .collect::<Vec<_>>()
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect()
    };
    let (a, b) = (grams(&a), grams(&b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let overlap = u32::try_from(a.intersection(&b).count()).unwrap_or(u32::MAX);
    let length = u32::try_from(a.len() + b.len()).unwrap_or(u32::MAX);
    2.0 * f64::from(overlap) / f64::from(length)
}

fn season_matches(candidate: &Candidate, season: u32) -> bool {
    let declared: Vec<_> = [&candidate.title, &candidate.japanese]
        .into_iter()
        .filter_map(|title| {
            let capture = SEASON.captures(title).ok().flatten()?;
            capture
                .get(1)
                .or_else(|| capture.get(2))?
                .as_str()
                .parse::<u32>()
                .ok()
        })
        .collect();
    if declared.is_empty() {
        season == 1
    } else {
        declared.iter().all(|declared| *declared == season)
    }
}

fn title_queries(name: &str, season: u32) -> Vec<String> {
    if season > 1 {
        let suffix = match season % 100 {
            11..=13 => "th",
            _ => match season % 10 {
                1 => "st",
                2 => "nd",
                3 => "rd",
                _ => "th",
            },
        };
        vec![
            format!("{name} Season {season}"),
            format!("{name} {season}{suffix} Season"),
        ]
    } else {
        let mut queries = vec![name.to_string()];
        if let Some((head, _)) = name.split_once(':') {
            queries.push(head.to_string());
        }
        queries
    }
}

fn title_score(titles: &[String], candidate: &Candidate) -> f64 {
    titles
        .iter()
        .flat_map(|title| {
            [&candidate.title, &candidate.japanese].map(|name| similarity(title, name))
        })
        .fold(0.0, f64::max)
}

fn valid_detail(page: &str, kind: MediaType, year: Option<u16>) -> bool {
    let document = Html::parse_document(page);
    let mut found_type = None;
    let mut found_year = None;
    for div in document.select(&selector("div")) {
        // Only direct text + span children, avoiding a whole-page ancestor
        // accidentally matching an unrelated show's metadata.
        let text = div.text().collect::<String>();
        if text.len() > 200 {
            continue;
        }
        if let Some(captures) = DETAILS.captures(&text).ok().flatten() {
            let (Some(label), Some(value)) = (captures.get(1), captures.get(2)) else {
                continue;
            };
            if label.as_str().eq_ignore_ascii_case("Type") {
                let value = value.as_str().to_ascii_lowercase();
                found_type = Some(if value.contains("movie") {
                    MediaType::Movie
                } else if value.starts_with("tv") {
                    MediaType::Series
                } else {
                    return false;
                });
            } else if found_year.is_none() {
                found_year = YEAR
                    .find(value.as_str())
                    .ok()
                    .flatten()
                    .and_then(|v| v.as_str().parse::<u16>().ok());
            }
        }
    }
    found_type == Some(kind) && year.is_none_or(|year| found_year == Some(year))
}

fn confident_match(mut candidates: Vec<(Candidate, f64)>) -> Option<Candidate> {
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    let selected = candidates.first()?;
    if selected.1 < 0.82
        || candidates
            .get(1)
            .is_some_and(|other| selected.1 - other.1 < 0.08)
    {
        return None;
    }
    Some(selected.0.clone())
}

fn episode_slug(html: &str, number: u32) -> Option<String> {
    let document = Html::parse_fragment(html);
    document
        .select(&EPISODES)
        .find(|a| {
            a.value()
                .attr("data-num")
                .and_then(|n| n.parse::<u32>().ok())
                == Some(number)
        })
        .map(|a| a.value().attr("data-slug").unwrap_or("").to_string())
        .map(|slug| {
            if slug.is_empty() {
                number.to_string()
            } else {
                slug
            }
        })
}

fn parse_servers(html: &str) -> Vec<Server> {
    let document = Html::parse_fragment(html);
    let mut servers = Vec::new();
    for group in document.select(&GROUPS) {
        let category = group.value().attr("data-type").unwrap_or_default();
        if !matches!(category, "sub" | "dub") {
            continue;
        }
        for li in group.select(&SERVERS) {
            if let Some(id) = li.value().attr("data-link-id").filter(|id| !id.is_empty()) {
                servers.push(Server {
                    id: id.into(),
                    label: li.text().collect::<String>().trim().into(),
                    dub: category == "dub",
                });
            }
        }
    }
    servers
}

async fn extract_server(ctx: &ResolveCtx<'_>, server: Server, referer: &str) -> Vec<Stream> {
    let Some(mut url) = site_url("/ajax/sources") else {
        return Vec::new();
    };
    url.query_pairs_mut()
        .append_pair("id", &server.id)
        .append_pair("asi", "0")
        .append_pair("autoPlay", "0");
    let Some(data) = ajax(ctx, url, referer).await else {
        return Vec::new();
    };
    let Some(embed) = data
        .get("url")
        .and_then(Value::as_str)
        .and_then(|url| Url::parse(url).ok())
    else {
        return Vec::new();
    };
    let parent = Url::parse(referer).ok();
    let ctx = ResolveCtx {
        fetcher: ctx.fetcher,
        media: ctx.media.clone(),
        source_id: Some("aniwaves"),
        referer: parent.as_ref(),
    };
    let echo = EchoVideo::new();
    let mega = Megaplay::new();
    let mut streams = if echo.supports(&ctx, &embed) {
        echo.extract(&ctx, &embed).await.unwrap_or_default()
    } else if mega.supports(&ctx, &embed) {
        mega.extract(&ctx, &embed).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    for stream in &mut streams {
        stream.label = Some(format!(
            "AniWaves {} · {}",
            if server.dub {
                "English DUB"
            } else {
                "Japanese SUB"
            },
            server.label
        ));
        stream.meta.dubbed = Some(server.dub);
        stream.meta.languages = vec![
            CountryCode::Multi,
            if server.dub {
                CountryCode::En
            } else {
                CountryCode::Ja
            },
        ];
        stream.meta.source_id = Some("aniwaves".into());
        stream.meta.source_label = Some("AniWaves".into());
        stream.ttl = stream.ttl.min(Duration::from_mins(5));
    }
    streams
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedFetcher;
    use std::sync::Arc;
    use vsources_core::traits::ResolvedMedia;
    use vsources_core::types::MediaId;

    #[tokio::test]
    async fn native_dub_pipeline_skips_sub_servers_and_never_downloads_media()
    -> Result<(), SourceError> {
        let mock=Arc::new(ScriptedFetcher::default()
            .page(|url| url.path()=="/filter","<a class='name d-title' href='/watch/frieren-4'>Frieren</a>")
            .page(|url| url.path()=="/watch/frieren-4","<div>Type: <span>TV</span></div><div>Date aired: <span>Sep 29, 2023</span></div>")
            .page(|url| url.path()=="/ajax/episode/list/4",serde_json::json!({"status":200,"result":"<a data-num='2' data-ids='s,d' data-slug='2'>Episode 2</a>"}).to_string())
            .page(|url| url.path()=="/ajax/server/list",serde_json::json!({"status":"200","result":"<div data-type='sub'><li data-link-id='s'>Vidplay</li></div><div data-type='dub'><li data-link-id='d'>Vidplay</li></div>"}).to_string())
            .page(|url| url.path()=="/ajax/sources" && url.query_pairs().any(|(k,v)|k=="id" && v=="d"),serde_json::json!({"status":200,"result":{"url":"https://play.echovideo.ru/embed-1/dub"}}).to_string())
            .page(|url| url.path()=="/embed-1/getSources",serde_json::json!({"sources":[{"file":"https://cdn.test/dub.m3u8","label":"720p"}]}).to_string()));
        let ctx = ResolveCtx {
            fetcher: mock.as_ref(),
            media: Some(ResolvedMedia {
                tmdb_id: Some(209_867),
                imdb_id: None,
                name: "Frieren".into(),
                year: Some(2023),
                season: Some(1),
                episode: Some(2),
            }),
            source_id: Some("aniwaves"),
            referer: None,
        };
        let provider = AniWaves::new();
        let streams = provider
            .resolve_english_dub(&ctx, &MediaRef::series(MediaId::Tmdb(209_867), 1, 2))
            .await?;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].meta.dubbed, Some(true));
        assert_eq!(streams[0].meta.resolution, Some(720));
        assert!(streams[0].meta.languages.contains(&CountryCode::En));
        assert!(!mock.requests().iter().any(|request| {
            request.url.path() == "/ajax/sources"
                && request
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "id" && v == "s")
        }));
        assert!(
            !mock
                .requests()
                .iter()
                .any(|request| request.url.host_str() == Some("cdn.test"))
        );
        assert_eq!(
            mock.header_sent_to("/embed-1/getSources", "Referer")
                .as_deref(),
            Some("https://play.echovideo.ru/embed-1/dub")
        );
        Ok(())
    }

    #[test]
    fn rejects_wrong_year_kind_and_ambiguous_series() {
        let page =
            "<div>Type: <span>TV</span></div><div>Date aired: <span>Sep 29, 2023</span></div>";
        assert!(valid_detail(page, MediaType::Series, Some(2023)));
        assert!(!valid_detail(page, MediaType::Series, Some(2020)));
        assert!(!valid_detail(page, MediaType::Movie, Some(2023)));
        let candidate = Candidate {
            id: 1,
            slug: "frieren-1".into(),
            title: "Frieren".into(),
            japanese: String::new(),
        };
        assert!(confident_match(vec![(candidate.clone(), 1.0), (candidate, 0.98)]).is_none());
    }

    #[test]
    fn later_season_queries_never_include_the_unqualified_title() {
        let queries = title_queries("Jujutsu Kaisen", 2);
        assert!(queries.iter().all(|q| q.contains('2')));
        assert!(similarity(&queries[0], "Jujutsu Kaisen Season 2") > 0.99);
        let mut candidate = Candidate {
            id: 1,
            slug: String::new(),
            title: "Frieren: Beyond Journey's End".into(),
            japanese: String::new(),
        };
        assert!(!season_matches(&candidate, 2));
        candidate.title.push_str(" 2nd Season");
        assert!(season_matches(&candidate, 2));
        assert!(!season_matches(&candidate, 1));
    }

    #[test]
    fn parses_exact_episode_and_keeps_dub_server_identity() {
        let episodes = "<a data-num='1' data-ids='one' data-slug='01'>One</a><a data-num='2' data-ids='two' data-slug='02'>Two</a>";
        assert_eq!(episode_slug(episodes, 2).as_deref(), Some("02"));
        assert_eq!(episode_slug(episodes, 3), None);
        let servers = parse_servers(
            "<div data-type='sub'><li data-link-id='s'>Vidplay</li></div><div data-type='dub'><li data-link-id='d'>Vidplay</li></div>",
        );
        assert_eq!(servers.len(), 2);
        assert!(!servers[0].dub);
        assert!(servers[1].dub);
        assert_eq!(servers[1].id, "d");
    }
}
