//! `EchoVideo`'s direct source API used by the current `AniWaves` player.
//!
//! The deployed `/embed-0`, `/embed-1` and `/embed-20` players expose
//! `getSources?id=...`. HLS sources and progressive quality maps are returned
//! directly; playback retains the player's origin as Referer.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream, SubtitleTrack};

/// Extract direct media from `EchoVideo` embeds without executing player scripts.
#[derive(Debug, Default)]
pub struct EchoVideo;

impl EchoVideo {
    /// A stateless extractor using the caller's fetcher.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn embed_parts(url: &Url) -> Option<(&str, &str)> {
    if url.host_str()? != "play.echovideo.ru" || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let (kind, id) = url.path().strip_prefix('/')?.split_once('/')?;
    if !matches!(kind, "embed-0" | "embed-1" | "embed-20")
        || id.is_empty()
        || id.len() > 512
        || id.contains('/')
    {
        return None;
    }
    Some((kind, id))
}

#[async_trait]
impl Extractor for EchoVideo {
    fn id(&self) -> &'static str {
        "echovideo"
    }

    fn label(&self) -> &'static str {
        "EchoVideo"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        embed_parts(url).is_some()
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let (kind, id) = embed_parts(url).ok_or(ExtractorError::NotFound)?;
        let mut endpoint = url.clone();
        endpoint.set_path(&format!("/{kind}/getSources"));
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        endpoint.query_pairs_mut().append_pair("id", id);
        let response = ctx
            .fetcher
            .request(
                FetchRequest::get(endpoint)
                    .with_header("Referer", url.as_str())
                    .with_header("X-Requested-With", "XMLHttpRequest")
                    .with_timeout(Duration::from_secs(8)),
            )
            .await?;
        let data: Value = response.json()?;
        let referer = format!("{}/", url.origin().ascii_serialization());
        let streams = parse_sources(&data, kind == "embed-20", &referer);
        if streams.is_empty() {
            Err(ExtractorError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

fn parse_sources(data: &Value, progressive: bool, referer: &str) -> Vec<Stream> {
    fn add(
        out: &mut Vec<Stream>,
        item: &Value,
        quality: Option<&str>,
        progressive: bool,
        referer: &str,
    ) {
        let Some(raw) = item.as_str().or_else(|| {
            item.get("file")
                .or_else(|| item.get("url"))
                .and_then(Value::as_str)
        }) else {
            return;
        };
        let Ok(url) = Url::parse(raw) else { return };
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || out.iter().any(|stream| stream.url == url)
        {
            return;
        }
        let format = if progressive {
            Format::Mp4
        } else {
            Format::Hls
        };
        let mut stream = Stream::new(url, format)
            .with_referer(referer)
            .with_ttl(Duration::from_mins(5));
        stream.meta.resolution =
            quality.and_then(|quality| quality.trim_end_matches('p').parse().ok());
        stream.meta.extractor_label = Some("EchoVideo".into());
        out.push(stream);
    }
    let mut out = Vec::new();
    match data.get("sources") {
        Some(Value::Array(items)) => {
            for item in items {
                add(
                    &mut out,
                    item,
                    item.get("label").and_then(Value::as_str),
                    progressive,
                    referer,
                );
            }
        }
        Some(Value::Object(qualities)) if progressive => {
            for (quality, items) in qualities {
                if let Some(items) = items.as_array() {
                    for item in items {
                        add(&mut out, item, Some(quality), true, referer);
                    }
                } else {
                    add(&mut out, items, Some(quality), true, referer);
                }
            }
        }
        Some(item) => add(&mut out, item, None, progressive, referer),
        None => {}
    }
    let subtitles: Vec<_> = data
        .get("tracks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|track| {
            if track.get("kind").and_then(Value::as_str) == Some("thumbnails") {
                return None;
            }
            let url = Url::parse(track.get("file").or_else(|| track.get("url"))?.as_str()?).ok()?;
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            Some(SubtitleTrack {
                url,
                language: track
                    .get("srclang")
                    .or_else(|| track.get("lang"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                label: track
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect();
    for stream in &mut out {
        stream.meta.subtitles.clone_from(&subtitles);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_shapes_keep_headers_quality_and_subtitles() {
        let streams = parse_sources(
            &json!({"sources":[{"file":"https://cdn.test/a.m3u8"},
            "https://cdn.test/a.m3u8",{"file":"file:///etc/passwd"}],
            "tracks":[{"file":"https://cdn.test/en.vtt","lang":"en","label":"English"}]}),
            false,
            "https://play.echovideo.ru/",
        );
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(streams[0].meta.subtitles.len(), 1);
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://play.echovideo.ru/")
        );
        let streams = parse_sources(
            &json!({"sources":{"720p":["https://cdn.test/a.mp4"]}}),
            true,
            "https://play.echovideo.ru/",
        );
        assert_eq!(streams[0].format, Format::Mp4);
        assert_eq!(streams[0].meta.resolution, Some(720));
    }

    #[test]
    fn only_the_supported_embed_host_and_routes_are_claimed() {
        for (raw, supported) in [
            ("https://play.echovideo.ru/embed-0/id", true),
            ("https://play.echovideo.ru/embed-20/id", true),
            ("https://evil.test/embed-0/id", false),
            ("https://play.echovideo.ru/embed-0/getSources/extra", false),
        ] {
            let url = Url::parse(raw).unwrap_or_else(|e| panic!("fixture: {e}"));
            assert_eq!(embed_parts(&url).is_some(), supported);
        }
    }
}
