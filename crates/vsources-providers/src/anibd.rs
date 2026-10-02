//! `AniBD`: direct anime Blu-ray HLS from `anibd.app`.
//!
//! Ports `src/source/AniBD.js` — a `WordPress` front over the public
//! `animeapps.top` API cluster with the `playeng.animeapps.top` CDN:
//!
//! 1. `AniList` GraphQL (`POST https://graphql.anilist.co`, scored
//!    candidate list — TV format, season-title awareness, year match);
//!    the top 3 candidates are walked until the episodes API yields
//!    servers, because a single `SEARCH_MATCH` can rank a spinoff ONA
//!    first (Task 41b upstream) whose epeng entry is empty;
//! 2. fallback text search
//!    `GET https://eng.animeapps.top/api/search3.php?keyword={title}`
//!    → `{data: [{postid, postname, anilist, postyear, …}]}` (the API
//!    indexes by Japanese romanization — postnames are scored with word
//!    overlap and a year bonus, and the query set includes the title
//!    split around its first colon);
//! 3. episodes `GET https://epeng.animeapps.top/api2.php?epid={anilistId}`
//!    → `[{id, server_name, server_data: [{name, slug, link}]}]` — the
//!    only server is `S-sub` (SUB-only site);
//! 4. resolve `GET https://epeng.animeapps.top/apilink.php?data={link}`
//!    → `[{server: "SR"|"SB", link:
//!    "https://playeng.animeapps.top/r2/play2.php?id=aniN&url={token}"}]`
//!    (only `SR` works — `SB` 404s);
//! 5. the direct playlist:
//!    `https://playeng.animeapps.top/r2/cachehd/{token}/index.m3u8`,
//!    which hotlink-gates on `Referer: https://anibd.app/`.
//!
//! `AniBD` streams are 1080p Blu-ray rips.
//!
//! Mappings and cuts (vs. upstream):
//! - The stream is direct — no extractor hop, no `/proxy` route; the
//!   `Referer` travels on `meta.request_headers` (upstream relied on the
//!   `AnimeDirect` extractor claiming `playeng.animeapps.top` URLs and
//!   re-routing them through the server proxy).
//! - `meta.title` (`{title} (Sub · {server_name})`) → [`Stream::label`];
//!   `meta.countryCodes` → `meta.languages`, `meta.height` →
//!   `meta.resolution`.
//! - The dead upstream `serverName` variable (assigned from the `AniList`
//!   romaji / `server_name` but never read) is not ported; a missing
//!   `server_name` falls back to `AniBD` like the upstream assignment
//!   intended.
//! - No result cache here — the parent's `CachedSource` wrapper owns it
//!   (upstream default 12h `this.ttl` → stream ttl).
//! - `getTmdbId`/`getTmdbNameAndYear` → `ctx.media`; missing media (no
//!   title to search) answers [`SourceError::NotFound`].
//! - The whole `AniList` stage is best-effort like the upstream `try`/
//!   `catch`: any failure falls through to the text search.
//! - Explicit `User-Agent` dropped — the fetcher sends browser-like
//!   headers. `http2: false` is wreq-internal and not ported.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; the normalizers still strip combining marks
//!   (U+0300–036F), and precomposed accents drop out.
//! - The `normalize` here also collapses doubled vowels
//!   (`Shippuuden` → `Shippuden`) to absorb romanization variants.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::traits::{FetchRequest, ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, Format, MediaRef, MediaType, SourceInfo, Stream};

/// The site root, upstream `BASE_URL`.
const BASE_URL: &str = "https://anibd.app";
/// The text-search API, upstream `SEARCH_API`.
const SEARCH_API: &str = "https://eng.animeapps.top/api/search3.php";
/// The episodes API, upstream `EPISODES_API`.
const EPISODES_API: &str = "https://epeng.animeapps.top/api2.php";
/// The embed-resolve API, upstream `APILINK_API`.
const APILINK_API: &str = "https://epeng.animeapps.top/apilink.php";
/// The playlist CDN root, upstream `PLAYENG_BASE`.
const PLAYENG_BASE: &str = "https://playeng.animeapps.top/r2/cachehd";
/// The hotlink Referer the CDN gates on.
const REFERER: &str = "https://anibd.app/";
/// The `AniList` GraphQL endpoint.
const ANILIST_GQL: &str = "https://graphql.anilist.co";
/// This provider's id, for scrape diagnostics.
const PROVIDER_ID: &str = "anibd";
/// `AniBD` rips are 1080p (`meta.height` upstream).
const HEIGHT: u16 = 1080;
/// Upstream request timeout for the API cluster.
const API_TIMEOUT: Duration = Duration::from_secs(15);
/// Upstream `AniList` timeout (`timeout: { request: 10000 }`).
const ANILIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Upstream result lifetime: the default 12h `this.ttl`.
const TTL: Duration = Duration::from_hours(12);

/// The `AniList` GraphQL query, verbatim from upstream.
const ANILIST_QUERY: &str = "query($search: String) { Page(perPage: 6) { media(search: $search, type: ANIME) { id title { romaji english } format episodes startDate { year } } } }";

/// An `AniList` GraphQL hit with its relevance score.
struct Candidate {
    /// `AniList` media id.
    id: u64,
    /// Relevance score (TV format, title, season awareness, year).
    score: i64,
}

/// The `anibd.app` provider: direct Blu-ray HLS.
pub struct AniBD {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// Static descriptor.
    info: SourceInfo,
}

impl AniBD {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// A new provider; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        let base = parse_url(BASE_URL);
        Self {
            mappings: None,
            info: SourceInfo {
                id: PROVIDER_ID.to_string(),
                label: "AniBD".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(base),
                priority: 0,
                // Upstream leaves `this.domainKey` unset.
                domain_key: None,
            },
        }
    }
}

impl Default for AniBD {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Source for AniBD {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // Upstream resolves TMDB (getTmdbId + getTmdbNameAndYear) and
        // searches by name; without pre-resolved media there is no title.
        let Some(resolved) = ctx.media.as_ref() else {
            return Err(SourceError::NotFound);
        };
        let title = display_title(&resolved.name, resolved.year, media);
        let target = target_episode(media);

        // Steps 1–2: the epeng server list, via AniList or text search.
        let Some(servers) = self.servers_for(ctx, resolved, media).await? else {
            return Err(SourceError::NotFound);
        };

        // Step 3: the only server ("S-sub") — SUB-only site.
        let Some(server) = servers.as_array().and_then(|list| list.first()) else {
            return Err(SourceError::NotFound);
        };
        let Some(link) = episode_link(server, target) else {
            return Err(SourceError::NotFound);
        };

        // Steps 4–5: apilink mirror → the direct playlist, hotlink-gated
        // on the anibd referer.
        let Some(playlist) = self.playlist_for(ctx, link).await? else {
            return Err(SourceError::NotFound);
        };
        let server_name = server
            .get("server_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("AniBD")
            .to_string();
        let mut stream = Stream::new(playlist, Format::Hls)
            .with_ttl(TTL)
            .with_referer(REFERER)
            .with_label(format!("{title} (Sub · {server_name})"));
        stream.meta.languages = vec![CountryCode::Multi, CountryCode::Ja];
        stream.meta.resolution = Some(HEIGHT);
        stream.meta.source_id = Some(self.info.id.clone());
        stream.meta.source_label = Some(self.info.label.clone());
        Ok(vec![stream])
    }
}

impl AniBD {
    /// Steps 1a/1b + 2: the `AniList` candidate walk against the episodes
    /// API, falling back to the text search. Upstream wraps the `AniList`
    /// stage in try/catch; a single `SEARCH_MATCH` can rank a spinoff ONA
    /// first, and the spinoff has no episodes.
    async fn servers_for(
        &self,
        ctx: &ResolveCtx<'_>,
        resolved: &ResolvedMedia,
        media: &MediaRef,
    ) -> Result<Option<Value>, SourceError> {
        if let Some(ids) = crate::anime_mapping::ids(self.mappings.as_ref(), ctx, media).await {
            let episodes = format!("{}?epid={}", EPISODES_API, ids.anilist_id);
            if let Ok(Some(found)) = api_get(ctx, &site_url(&episodes)?).await
                && found
                    .as_array()
                    .and_then(|list| list.first())
                    .and_then(|first| first.get("server_data"))
                    .and_then(Value::as_array)
                    .is_some_and(|data| !data.is_empty())
            {
                return Ok(Some(found));
            }
        }
        let candidates = anilist_candidates(ctx, &resolved.name, media.season, resolved.year)
            .await
            .unwrap_or_default();
        for candidate in candidates.iter().take(3) {
            let episodes = format!("{}?epid={}", EPISODES_API, candidate.id);
            let Some(found) = api_get(ctx, &site_url(&episodes)?).await? else {
                continue;
            };
            // JS: `Array.isArray(srv) && srv.length > 0 && srv[0]?.server_data?.length`.
            if let Some(first) = found.as_array().and_then(|list| list.first())
                && first
                    .pointer("/server_data")
                    .and_then(Value::as_array)
                    .is_some_and(|data| !data.is_empty())
            {
                return Ok(Some(found));
            }
        }

        // Step 1b: AniList lookup failed → text search.
        let Some(anime) = self.find_anime(ctx, &resolved.name, resolved.year).await? else {
            return Ok(None);
        };
        let episodes = format!("{}?epid={}", EPISODES_API, anime.anilist_id);
        api_get(ctx, &site_url(&episodes)?).await
    }

    /// Steps 4–5: resolve the embed (playerDataId) via apilink, pick the
    /// SR mirror (SB is dead — 404s), and build the playlist from its
    /// `url` token.
    async fn playlist_for(
        &self,
        ctx: &ResolveCtx<'_>,
        link: &str,
    ) -> Result<Option<Url>, SourceError> {
        let mirrors = format!("{}?data={}", APILINK_API, encode_component(link));
        let Some(mirrors) = api_get(ctx, &site_url(&mirrors)?).await? else {
            return Ok(None);
        };
        let Some(mirror_list) = mirrors.as_array() else {
            return Ok(None);
        };
        let mirror = mirror_list
            .iter()
            .find(|mirror| mirror.get("server").and_then(Value::as_str) == Some("SR"))
            .or_else(|| mirror_list.first());
        let Some(mirror) = mirror else {
            return Ok(None);
        };
        let Some(mirror_link) = mirror.get("link").and_then(Value::as_str) else {
            return Ok(None);
        };

        // The token rides in the mirror link's `url` query param.
        let mirror_url = site_url(mirror_link)?;
        let Some(token) = mirror_url
            .query_pairs()
            .find(|(key, _)| key == "url")
            .map(|(_, value)| value.to_string())
        else {
            return Ok(None);
        };
        Ok(Some(site_url(&format!(
            "{PLAYENG_BASE}/{token}/index.m3u8"
        ))?))
    }

    /// Step 1b: text search — `{postid, anilist}` of the best match
    /// (score ≥ 40; word overlap ≥ 0.4 because TMDB titles use the
    /// English prefix while the API indexes Japanese romanization, and
    /// a ±1-year match is strong confirmation).
    async fn find_anime(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        year: Option<u16>,
    ) -> Result<Option<TextHit>, SourceError> {
        let name_norm = normalize(name);
        let name_words: HashSet<String> = name_norm
            .split(' ')
            .filter(|word| word.chars().count() > 2)
            .map(str::to_string)
            .collect();
        for query in query_variants(name) {
            let search = format!("{}?keyword={}", SEARCH_API, encode_component(&query));
            let Some(payload) = api_get(ctx, &site_url(&search)?).await? else {
                continue;
            };
            let Some(results) = payload.get("data").and_then(Value::as_array) else {
                continue;
            };

            let mut best: Option<TextHit> = None;
            let mut best_score = 0.0_f64;
            for result in results {
                let raw_postname = result
                    .get("postname")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let titles = [cleaned_postname(raw_postname), raw_postname.to_string()];
                let mut item_best = 0.0_f64;
                for candidate in titles {
                    let title_norm = normalize(&candidate);
                    if title_norm.is_empty() {
                        continue;
                    }
                    let mut score = if title_norm == name_norm {
                        100.0
                    } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
                        inclusion_score(&title_norm, &name_norm)
                    } else {
                        0.0
                    };
                    // Word-overlap scoring, threshold 0.4 (lowered from
                    // 0.6 — "Demon Slayer" ↔ "Kimetsu no Yaiba" share
                    // only "kimetsu" and "yaiba": 50%, still a strong
                    // match with the year bonus below).
                    if score < 50.0 && name_words.len() >= 2 {
                        let title_words: HashSet<&str> = title_norm
                            .split(' ')
                            .filter(|word| word.chars().count() > 2)
                            .collect();
                        let common = name_words
                            .iter()
                            .filter(|word| title_words.contains(word.as_str()))
                            .count();
                        let overlap =
                            count_f64(common) / count_f64(name_words.len().max(title_words.len()));
                        if overlap >= 0.4 {
                            score = overlap * 80.0;
                        }
                    }
                    // Year matching — strong confirmation, strong negative.
                    if score > 0.0
                        && let Some(year) = year
                        && let Some(post_year) = result
                            .get("postyear")
                            .and_then(Value::as_str)
                            .and_then(leading_int)
                    {
                        let distance = (post_year - i64::from(year)).abs();
                        if distance <= 1 {
                            score += 25.0;
                        } else if distance > 2 {
                            score -= 15.0;
                        }
                    }
                    item_best = item_best.max(score);
                }
                if item_best > best_score {
                    best_score = item_best;
                    best = Some(TextHit {
                        anilist_id: result
                            .get("anilist")
                            .map(Value::to_string)
                            .unwrap_or_default(),
                    });
                }
            }

            // Threshold 40 (lowered from 50): subtitled TMDB titles
            // match the romanized postnames weakly even with the year.
            if let Some(best) = best
                && best_score >= 40.0
            {
                return Ok(Some(best));
            }
        }
        Ok(None)
    }
}

/// Step 3: the requested episode's playerDataId in the single `S-sub`
/// server — a `parseInt(ep.name)` match, movies falling back to the
/// first episode.
fn episode_link(server: &Value, target: i64) -> Option<&str> {
    let server_data = server.pointer("/server_data")?.as_array()?;
    if server_data.is_empty() {
        return None;
    }
    let episode = server_data
        .iter()
        .find(|ep| {
            ep.get("name")
                .and_then(Value::as_str)
                .and_then(leading_int)
                .is_some_and(|number| number == target)
        })
        .or_else(|| server_data.first())?;
    episode.get("link").and_then(Value::as_str)
}

/// A text-search hit: the `AniList` id (epeng's `epid`).
struct TextHit {
    /// The API's `anilist` field, stringified like upstream.
    anilist_id: String,
}

/// `AniList` GraphQL candidates, scored and sorted — ports
/// `getAniListCandidates`. The AniList-specific normalizer strips
/// apostrophes (curly and straight) and collapses non-alphanumerics.
async fn anilist_candidates(
    ctx: &ResolveCtx<'_>,
    name: &str,
    season: Option<u32>,
    year: Option<u16>,
) -> Result<Vec<Candidate>, SourceError> {
    let body =
        serde_json::json!({ "query": ANILIST_QUERY, "variables": { "search": name } }).to_string();
    let request = FetchRequest::post(parse_url(ANILIST_GQL), body)
        .with_timeout(ANILIST_TIMEOUT)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
    let response = match ctx.fetcher.request(request).await {
        Ok(response) if response.status == 200 => response,
        Ok(_) => return Ok(Vec::new()),
        Err(error) if is_miss(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let Ok(data) = response.json::<Value>() else {
        return Ok(Vec::new());
    };

    let name_norm = anilist_norm(name);
    let year_num = year.map(i64::from);
    let mut candidates: Vec<Candidate> = data
        .pointer("/data/Page/media")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|media| {
            let english = media
                .pointer("/title/english")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let romaji = media
                .pointer("/title/romaji")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // `const bestTitle = eng || romaji`.
            let best_title = if english.is_empty() { romaji } else { english };
            let title_norm = anilist_norm(best_title);
            let mut score = 0;
            // TV series rank above ONAs/OVAs.
            if media.get("format").and_then(Value::as_str) == Some("TV") {
                score += 40;
            }
            if title_norm == name_norm {
                score += 30;
            } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
                score += 15;
            }
            // Season-title awareness: "... Season 2" entries.
            let mention = season_mention(&title_norm);
            if let Some(season) = season.filter(|season| *season > 1) {
                match mention {
                    Some(mentioned) if mentioned == i64::from(season) => score += 20,
                    Some(_) => score -= 15,
                    None => {}
                }
            } else if mention.is_some() {
                score -= 10;
            }
            let media_year = media.pointer("/startDate/year").and_then(Value::as_i64);
            if let (Some(year), Some(media_year)) = (year_num, media_year) {
                if (media_year - year).abs() <= 1 {
                    score += 10;
                } else if (media_year - year).abs() > 2 {
                    score -= 10;
                }
            }
            Candidate {
                id: media.get("id").and_then(Value::as_u64).unwrap_or_default(),
                score,
            }
        })
        .filter(|candidate| candidate.id != 0)
        .collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    Ok(candidates)
}

/// `apiGet`: JSON from the animeapps cluster, or `None` on a miss
/// (upstream returns `null` for non-200 and for malformed JSON).
/// Transport failures propagate — upstream `gotScraping` throws.
async fn api_get(ctx: &ResolveCtx<'_>, url: &Url) -> Result<Option<Value>, SourceError> {
    let request = FetchRequest::get(url.clone())
        .with_timeout(API_TIMEOUT)
        .with_header("Accept", "application/json")
        .with_header("Referer", format!("{BASE_URL}/"));
    let response = match ctx.fetcher.request(request).await {
        Ok(response) if response.status == 200 => response,
        Ok(_) => return Ok(None),
        Err(error) if is_miss(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(response.json::<Value>().ok())
}

/// The AniList-stage normalizer — apostrophes removed, non-alphanumeric
/// runs collapsed to single spaces.
fn anilist_norm(s: &str) -> String {
    let lowered: String = s.to_lowercase().replace(['\u{2019}', '\''], "");
    let mut out = String::with_capacity(lowered.len());
    let mut pending_space = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// A trailing `Season N` mention in a normalized title, if any.
fn season_mention(norm_title: &str) -> Option<i64> {
    let mut start = 0;
    while let Some(offset) = norm_title[start..].find("season") {
        let after = &norm_title[start + offset + "season".len()..];
        let digits: String = after
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(number) = digits.parse() {
            return Some(number);
        }
        start += offset + "season".len();
    }
    None
}

/// Module normalizer — like the shared one but collapsing doubled
/// vowels (`uu|oo|aa|ee|ii` → one) to absorb romanization variants
/// ("Shippuuden" → "Shippuden").
fn normalize(s: &str) -> String {
    collapse_vowels(&base_normalize(s))
}

/// The vowel-collapse pass, ports `.replace(/(uu|oo|aa|ee|ii)/g, m => m[0])`.
fn collapse_vowels(s: &str) -> String {
    const DOUBLES: [&str; 5] = ["uu", "oo", "aa", "ee", "ii"];
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() {
            let pair: String = chars[i..i + 2].iter().collect();
            if DOUBLES.contains(&pair.as_str()) {
                out.push(chars[i]);
                i += 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The shared normalize shape: lowercase, strip combining marks, drop
/// non-alphanumerics, collapse whitespace.
fn base_normalize(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Search query variants — the name as-is, diacritics-stripped,
/// punctuation-spaced, plus both halves of the first colon (TMDB
/// titles carry subtitles while the API indexes romanizations).
fn query_variants(name: &str) -> Vec<String> {
    let stripped: String = name
        .chars()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .collect();
    let spaced: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.is_ascii_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let spaced = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut variants: Vec<String> = vec![name.to_string(), stripped, spaced];
    if let Some(colon) = name.find(':')
        && colon > 0
    {
        variants.push(name[..colon].trim().to_string());
        variants.push(name[colon + 1..].trim().to_string());
    }
    let mut unique: Vec<String> = Vec::new();
    for query in variants {
        if !query.is_empty() && !unique.contains(&query) {
            unique.push(query);
        }
    }
    unique
}

/// The postname cleaner — strip a trailing `BD|TV|Movie|OVA|ONA|Special`
/// marker, then `[():]` → spaces, collapsed.
fn cleaned_postname(raw: &str) -> String {
    let stripped = strip_release_suffix(raw);
    let spaced: String = stripped
        .chars()
        .map(|c| if matches!(c, '(' | ')' | ':') { ' ' } else { c })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip the trailing release marker (with the whitespace the upstream
/// regex requires before it).
fn strip_release_suffix(raw: &str) -> &str {
    const SUFFIXES: [&str; 6] = ["bd", "tv", "movie", "ova", "ona", "special"];
    let trimmed = raw.trim_end();
    for suffix in SUFFIXES {
        if let Some(cut) = trimmed.len().checked_sub(suffix.len())
            && trimmed.is_char_boundary(cut)
        {
            let (head, tail) = trimmed.split_at(cut);
            if tail.eq_ignore_ascii_case(suffix) && head.ends_with(char::is_whitespace) {
                return head.trim_end();
            }
        }
    }
    trimmed
}

/// One-sided containment score: 90·(min/max) of the normalized lengths.
fn inclusion_score(a: &str, b: &str) -> f64 {
    let (a_len, b_len) = (len_f64(a), len_f64(b));
    a_len.min(b_len) / a_len.max(b_len) * 90.0
}

/// A char count as an exact `f64` (clamped at `u32::MAX`; normalized
/// titles are far below), avoiding a lossy `usize` cast.
fn len_f64(s: &str) -> f64 {
    f64::from(u32::try_from(s.chars().count()).unwrap_or(u32::MAX))
}

/// A count as an exact `f64` (clamped at `u32::MAX`), avoiding a lossy
/// `usize` cast.
fn count_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// The display title — `name S01E02` for episodes, `name (year)` for
/// movies (ports `getTmdbNameAndYear` + `formatSeasonAndEpisode`).
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{name} {}", media.format_season_and_episode())
    } else {
        format!(
            "{name} ({})",
            year.map(|y| y.to_string()).unwrap_or_default()
        )
    }
}

/// The episode number to look for — the reference's episode for series,
/// 1 for movies (`tmdbId.season ? (tmdbId.episode || 1) : 1`).
fn target_episode(media: &MediaRef) -> i64 {
    i64::from(media.season.map_or(1, |_| media.episode.unwrap_or(1)))
}

/// `parseInt`-style leading-integer parse for numeric string fields.
fn leading_int(s: &str) -> Option<i64> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Percent-encode like the JS `encodeURIComponent`.
fn encode_component(s: &str) -> String {
    /// The uppercase hex digits for percent escapes.
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
    ];
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)]);
            out.push(HEX[usize::from(byte & 0x0F)]);
        }
    }
    out
}

/// Parse a runtime-built URL — a structural surprise, not a miss.
fn site_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw).map_err(|error| {
        SourceError::scrape(PROVIDER_ID, format!("unparsable URL {raw:?}: {error}"))
    })
}

/// A constant URL — must parse.
fn parse_url(raw: &str) -> Url {
    Url::parse(raw).unwrap_or_else(|e| panic!("the AniBD URL {raw:?} must parse: {e}"))
}
/// Whether a fetch failure is a miss — upstream `throwHttpErrors:
/// false` turns non-200 answers into `null`, transport errors throw.
fn is_miss(error: &FetchError) -> bool {
    matches!(
        error,
        FetchError::NotFound { .. }
            | FetchError::Http { .. }
            | FetchError::RateLimited { .. }
            | FetchError::Blocked { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::MediaId;

    /// The searched title.
    const NAME: &str = "Frieren: Beyond Journey's End";

    /// A fetcher serving canned bodies keyed by `host + path?query` and
    /// recording every request (the extractors' `ScriptedFetcher` pattern
    /// — `pub(crate)` there, so a per-module copy lives here).
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, String>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Serve `url` (host + path + query) with `body`.
        fn page(self, url: impl Into<String>, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(url.into(), body.into());
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

    /// The mock's page key: host + path + query.
    fn key_of(url: &Url) -> String {
        let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        format!(
            "{}{}{}",
            url.host_str().unwrap_or_default(),
            url.path(),
            query
        )
    }

    /// The page key for a URL string, canonicalized exactly like the
    /// provider's wire requests (the `url` crate percent-encodes some
    /// query characters, e.g. `'`).
    fn url_key(raw: &str) -> String {
        let url = Url::parse(raw).unwrap_or_else(|e| panic!("valid URL {raw:?}: {e}"));
        key_of(&url)
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let key = key_of(&request.url);
            let body = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned();
            match body {
                Some(body) => Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: std::collections::BTreeMap::from([(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// Resolved TMDB metadata for the searched title.
    fn resolved_media(season: Option<u32>, episode: Option<u32>) -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(154_587),
            imdb_id: None,
            name: NAME.to_string(),
            year: Some(2023),
            season,
            episode,
        }
    }

    /// The media reference matching [`resolved_media`].
    fn media_ref(season: Option<u32>, episode: Option<u32>) -> MediaRef {
        MediaRef {
            id: MediaId::Tmdb(154_587),
            kind: MediaType::Series,
            season,
            episode,
        }
    }

    /// A resolve context over the scripted fetcher.
    fn ctx_for(fetcher: &ScriptedFetcher, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// The `AniList` GraphQL fixture: the main TV series first.
    const ANILIST_PAGE: &str = r#"{"data":{"Page":{"media":[
        {"id":154587,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End"},"format":"TV","episodes":28,"startDate":{"year":2023}},
        {"id":170068,"title":{"romaji":"Sousou no Frieren: Mahou","english":null},"format":"ONA","episodes":null,"startDate":{"year":2026}}
    ]}}}"#;

    /// The epeng episodes fixture (server "S-sub", episodes 1 and 2).
    const EPISODES_PAGE: &str = r#"[{"id":1,"server_name":"S-sub","server_data":[
        {"name":"1","slug":"ep-1","link":"pdi1"},
        {"name":"2","slug":"ep-2","link":"pdi2"}
    ]}]"#;

    /// The apilink fixture: the working SR mirror.
    const APILINK_PAGE: &str = r#"[{"server":"SR","link":"https://playeng.animeapps.top/r2/play2.php?id=aniN&url=TOKEN123"},{"server":"SB","link":"https://playeng.animeapps.top/r2/play2.php?id=aniM&url=DEAD"}]"#;

    #[tokio::test]
    async fn builds_the_direct_playlist_from_anilist() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page("graphql.anilist.co/", ANILIST_PAGE)
            .page("epeng.animeapps.top/api2.php?epid=154587", EPISODES_PAGE)
            .page("epeng.animeapps.top/apilink.php?data=pdi2", APILINK_PAGE);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AniBD::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the AniList path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://playeng.animeapps.top/r2/cachehd/TOKEN123/index.m3u8"
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · S-sub)")
        );
        assert_eq!(
            stream.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.source_id.as_deref(), Some("anibd"));
        assert_eq!(stream.meta.source_label.as_deref(), Some("AniBD"));
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://anibd.app/")
        );
        // The GraphQL POST carried the search name.
        assert!(fetcher.requests().iter().any(|request| {
            request.method == "POST"
                && request
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains(NAME))
        }));
        // The API calls carried the anibd Referer.
        assert!(fetcher.requests().iter().any(|request| {
            request.url.as_str() == "https://epeng.animeapps.top/api2.php?epid=154587"
                && request
                    .headers
                    .get("Referer")
                    .is_some_and(|referer| referer == "https://anibd.app/")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn movies_take_the_first_episode() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page("graphql.anilist.co/", ANILIST_PAGE)
            .page("epeng.animeapps.top/api2.php?epid=154587", EPISODES_PAGE)
            .page("epeng.animeapps.top/apilink.php?data=pdi1", APILINK_PAGE);
        let ctx = ctx_for(&fetcher, Some(resolved_media(None, None)));
        let media = media_ref(None, None);

        let streams = AniBD::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the movie path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End (2023) (Sub · S-sub)")
        );
        assert!(
            streams[0]
                .url
                .as_str()
                .ends_with("/cachehd/TOKEN123/index.m3u8")
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_candidates_walk_to_the_next_one() -> Result<(), SourceError> {
        // The better-scoring candidate has no servers; the second wins.
        let fetcher = ScriptedFetcher::new()
            .page("graphql.anilist.co/", ANILIST_PAGE)
            .page("epeng.animeapps.top/api2.php?epid=154587", "[]")
            .page("epeng.animeapps.top/api2.php?epid=170068", EPISODES_PAGE)
            .page("epeng.animeapps.top/apilink.php?data=pdi2", APILINK_PAGE);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AniBD::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the candidate walk must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert!(
            fetcher.requests().iter().any(|request| request.url.as_str()
                == "https://epeng.animeapps.top/api2.php?epid=170068")
        );
        Ok(())
    }

    #[tokio::test]
    async fn anilist_failure_falls_back_to_text_search() -> Result<(), SourceError> {
        // AniList is down; the TMDB title carries a subtitle, and only
        // the after-colon query variant (the romanization) is indexed.
        let name = "Demon Slayer: Kimetsu no Yaiba";
        let fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://eng.animeapps.top/api/search3.php?keyword={}",
                    encode_component("Kimetsu no Yaiba")
                )),
                r#"{"data":[{"postid":9,"postname":"Kimetsu no Yaiba BD","anilist":21,"postyear":"2019"}]}"#,
            )
            .page("epeng.animeapps.top/api2.php?epid=21", EPISODES_PAGE)
            .page("epeng.animeapps.top/apilink.php?data=pdi2", APILINK_PAGE);
        let media = MediaRef {
            id: MediaId::Tmdb(21),
            kind: MediaType::Series,
            season: Some(1),
            episode: Some(2),
        };
        let ctx = ctx_for(
            &fetcher,
            Some(ResolvedMedia {
                tmdb_id: Some(21),
                imdb_id: None,
                name: name.to_string(),
                year: Some(2019),
                season: Some(1),
                episode: Some(2),
            }),
        );

        let streams = AniBD::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the text-search fallback must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        // The colon-split variant found the romanized postname via word
        // overlap (50%) plus the year bonus (25) — over the threshold 40.
        assert!(fetcher.requests().iter().any(|request| request.url.as_str()
            == "https://eng.animeapps.top/api/search3.php?keyword=Kimetsu%20no%20Yaiba"));
        Ok(())
    }

    #[tokio::test]
    async fn a_search_without_a_match_is_not_found() {
        // AniList and every query variant miss.
        let fetcher = ScriptedFetcher::new().page(
            url_key(&format!(
                "https://eng.animeapps.top/api/search3.php?keyword={}",
                encode_component(NAME)
            )),
            r#"{"data":[]}"#,
        );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match AniBD::new().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a search miss must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let ctx = ctx_for(&fetcher, None);
        let media = media_ref(Some(1), Some(2));

        match AniBD::new().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("no resolved media must be NotFound, got {other:?}"),
        }
    }

    #[test]
    fn normalizes_and_collapses_doubled_vowels() {
        for (input, expected) in [
            ("Shippuuden", "shippuden"),
            ("Sōusou no Frieren", "susou no frieren"),
            ("  Kimetsu  no Yaiba ", "kimetsu no yaiba"),
        ] {
            assert_eq!(normalize(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn anilist_norm_strips_apostrophes() {
        for (input, expected) in [
            (
                "Frieren: Beyond Journey's End",
                "frieren beyond journeys end",
            ),
            ("Sousou’s Tale", "sousous tale"),
        ] {
            assert_eq!(anilist_norm(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn finds_season_mentions() {
        for (input, expected) in [
            ("sousou no frieren season 2", Some(2)),
            ("mushoku tensei jobless reincarnation part 2", None),
            ("title season2", Some(2)),
        ] {
            assert_eq!(season_mention(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn cleans_postnames() {
        for (input, expected) in [
            ("Kimetsu no Yaiba BD", "Kimetsu no Yaiba"),
            ("Kimetsu no Yaiba (TV)", "Kimetsu no Yaiba TV"),
            ("Owarimonogatari", "Owarimonogatari"),
            ("Movie (2023)", "Movie 2023"),
        ] {
            assert_eq!(cleaned_postname(input), expected, "input {input:?}");
        }
    }
}
