//! `VidKing` embeds and media-keyed fallbacks via the shared speedracelight API.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx, ResolvedMedia};
use vsources_core::types::{CountryCode, Stream};

use crate::helpers::{direct_stream, format_for_url};
use crate::speedracelight::{self, ProviderQuery, SeedStore};

/// The `VidKing` extractor, including a shared result cache by media identity.
pub struct VidKing {
    seeds: Arc<SeedStore>,
    results: Cache<String, Arc<Vec<Stream>>>,
}

impl Default for VidKing {
    fn default() -> Self {
        Self::new()
    }
}

impl VidKing {
    /// Build a resolver with an independent seed and media cache.
    pub fn new() -> Self {
        Self::with_seeds(Arc::new(SeedStore::new()))
    }

    /// Share seeds with other consumers of the speedracelight API.
    pub fn with_seeds(seeds: Arc<SeedStore>) -> Self {
        Self {
            seeds,
            results: Cache::builder()
                .max_capacity(256)
                .time_to_live(Duration::from_mins(5))
                .build(),
        }
    }
}

#[async_trait]
impl Extractor for VidKing {
    fn id(&self) -> &'static str {
        "vidking"
    }
    fn label(&self) -> &'static str {
        "VidKing"
    }
    fn supports(&self, _: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|h| h == "vidking.net" || h.ends_with(".vidking.net"))
            && embed_media(url).is_some()
    }
    fn normalize(&self, url: &Url) -> Url {
        let mut url = url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url
    }
    fn cache_version(&self) -> Option<u32> {
        Some(1)
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let media = metadata(ctx, url).await?;
        let id = media.tmdb_id.ok_or(ExtractorError::NotFound)?;
        let key = format!(
            "{id}:{:?}:{:?}:{:?}:{:?}:{:?}",
            media.season, media.episode, media.name, media.year, media.imdb_id
        );
        let result = self
            .results
            .try_get_with(key, async {
                let query = ProviderQuery {
                    title: media.name.clone(),
                    year: media.year,
                    media_type: if media.season.is_some() {
                        "tv"
                    } else {
                        "movie"
                    }
                    .into(),
                    tmdb_id: id,
                    imdb_id: media.imdb_id.clone(),
                    season_id: media.season.unwrap_or(1),
                    episode_id: media.episode.unwrap_or(1),
                };
                let payloads = speedracelight::fetch_all_providers(ctx, &self.seeds, &query).await;
                let streams = assemble(&payloads);
                if streams.is_empty() {
                    Err(ExtractorError::NotFound)
                } else {
                    Ok(Arc::new(streams))
                }
            })
            .await
            .map_err(|e| (*e).clone())?;
        // Cached payloads are caller-independent. Decorate a fresh copy.
        Ok(result
            .iter()
            .cloned()
            .map(|mut stream| {
                stream.meta.source_id = ctx.source_id.map(str::to_owned);
                stream
            })
            .collect())
    }
}

fn embed_media(url: &Url) -> Option<ResolvedMedia> {
    let parts: Vec<_> = url.path_segments()?.collect();
    if parts.first().copied() != Some("embed") {
        return None;
    }
    let kind = *parts.get(1)?;
    let id: u64 = parts.get(2)?.parse().ok()?;
    let (season, episode) = match kind {
        "movie" if parts.len() == 3 => (None, None),
        "tv" if parts.len() == 5 => (
            Some(parts[3].parse::<u32>().ok()?),
            Some(parts[4].parse::<u32>().ok()?),
        ),
        _ => return None,
    };
    if id == 0 || season == Some(0) || episode == Some(0) {
        return None;
    }
    Some(ResolvedMedia {
        tmdb_id: Some(id),
        imdb_id: None,
        name: String::new(),
        year: None,
        season,
        episode,
    })
}

async fn metadata(ctx: &ResolveCtx<'_>, url: &Url) -> Result<ResolvedMedia, ExtractorError> {
    let mut media = ctx
        .media
        .clone()
        .filter(|m| m.tmdb_id.is_some())
        .or_else(|| embed_media(url))
        .ok_or(ExtractorError::NotFound)?;
    if !media.name.is_empty() {
        return Ok(media);
    }
    // Standalone CLI extraction has no preloaded metadata. Credentials stay
    // optional and environment-only, exactly like the engine's TMDB client.
    let kind = if media.season.is_some() {
        "tv"
    } else {
        "movie"
    };
    let id = media.tmdb_id.ok_or(ExtractorError::NotFound)?;
    let mut target = Url::parse(&format!("https://api.themoviedb.org/3/{kind}/{id}"))
        .map_err(|_| ExtractorError::NotFound)?;
    let mut request = FetchRequest::get(target.clone());
    if let Some(token) = std::env::var("TMDB_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
    {
        request = request.with_header("Authorization", format!("Bearer {token}"));
    } else if let Some(key) = std::env::var("TMDB_API_KEY").ok().filter(|s| !s.is_empty()) {
        target.query_pairs_mut().append_pair("api_key", &key);
        request.url = target;
    } else {
        return Err(ExtractorError::NotFound);
    }
    let data: Value = ctx.fetcher.request(request).await?.json()?;
    media.name = data
        .get("title")
        .or_else(|| data.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(ExtractorError::NotFound)?
        .to_string();
    media.year = data
        .get("release_date")
        .or_else(|| data.get("first_air_date"))
        .and_then(Value::as_str)
        .and_then(|s| s.get(..4))
        .and_then(|s| s.parse().ok());
    media.imdb_id = data
        .get("imdb_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(media)
}

fn assemble(payloads: &[(speedracelight::Provider, Option<Value>)]) -> Vec<Stream> {
    let mut seen = HashSet::new();
    let mut streams = Vec::new();
    let origin =
        Url::parse("https://www.vidking.net/").unwrap_or_else(|e| panic!("constant URL: {e}"));
    for (provider, payload) in payloads {
        let Some(sources) = payload
            .as_ref()
            .and_then(|p| p.get("sources"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for source in sources {
            let Some(url) = source
                .get("url")
                .and_then(Value::as_str)
                .and_then(|s| Url::parse(s).ok())
            else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") || !seen.insert(url.clone()) {
                continue;
            }
            let format = format_for_url(&url);
            let no_referer = url.host_str().is_some_and(|h| {
                ["vimeos.net", "vimeos.zip"]
                    .iter()
                    .any(|suffix| h == *suffix || h.ends_with(&format!(".{suffix}")))
            });
            let mut stream = if no_referer {
                Stream::new(url, format).with_ttl(Duration::from_mins(5))
            } else {
                direct_stream(url, format, Duration::from_mins(5), &origin)
            };
            let quality = source
                .get("quality")
                .map(|q| q.as_str().map_or_else(|| q.to_string(), str::to_owned))
                .unwrap_or_default();
            stream.label = Some(format!("{} {quality}", provider.name).trim().into());
            stream.meta.resolution = quality
                .split(|c: char| !c.is_ascii_digit())
                .find_map(|n| n.parse::<u16>().ok().filter(|n| (240..=4320).contains(n)));
            stream.meta.quality = (!quality.is_empty()).then_some(quality.clone());
            let lower = quality.to_ascii_lowercase();
            for (name, country) in [
                ("english", CountryCode::En),
                ("hindi", CountryCode::Hi),
                ("german", CountryCode::De),
                ("tamil", CountryCode::Ta),
                ("telugu", CountryCode::Te),
            ] {
                if lower.contains(name) {
                    stream.meta.languages.push(country);
                }
            }
            streams.push(stream);
        }
    }
    streams
}

#[cfg(test)]
mod tests;
