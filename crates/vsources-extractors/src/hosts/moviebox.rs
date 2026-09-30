//! `MovieBox`: direct streams from the aoneroom download API.
//!
//! Ports `src/extractor/MovieBox.js`. The embed URL's
//! `subjectId`/`se`/`ep`/`detailPath` parameters query the h5 download
//! API, whose `downloads` list becomes one stream per entry. Playback
//! hotlinks are gated on `Referer: https://videodownloader.site/`, the
//! same origin the API queries carry.
//!
//! Cut from the upstream port: the `meta.countryCodes` passthrough —
//! [`ResolveCtx`] carries no country codes, so every stream defaults to
//! `multi` like the upstream default.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{CountryCode, Format, Stream};

const API_BASE_URL: &str = "https://h5-api.aoneroom.com";
const DOWNLOAD_PATH: &str = "/wefeed-h5api-bff/subject/download";
/// The API and playback hotlink Referer.
const API_REFERER: &str = "https://videodownloader.site/";

/// Upstream result lifetime: 3h.
const TTL: Duration = Duration::from_hours(3);

/// One entry of the API's `downloads` list.
#[derive(Debug)]
struct Download {
    resolution: Option<u64>,
    url: String,
    format: Option<String>,
    size: Option<String>,
}

/// The `MovieBox` family extractor.
#[derive(Debug, Default)]
pub struct MovieBox;

impl MovieBox {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for MovieBox {
    fn id(&self) -> &'static str {
        "moviebox"
    }

    fn label(&self) -> &'static str {
        "MovieBox"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| host.contains("moviebox") || host.contains("aoneroom"))
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let subject_id = query_param(url, "subjectId").ok_or(ExtractorError::NotFound)?;
        let se = query_param(url, "se").unwrap_or_else(|| "0".to_string());
        let ep = query_param(url, "ep").unwrap_or_else(|| "0".to_string());
        let detail_path = query_param(url, "detailPath");

        // `new URL(`${API_BASE_URL}${DOWNLOAD_PATH}`)` with the query
        // params set in upstream order.
        let mut download_url = Url::parse(&format!("{API_BASE_URL}{DOWNLOAD_PATH}"))
            .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid API URL: {e}")))?;
        {
            let mut query = download_url.query_pairs_mut();
            query.append_pair("subjectId", &subject_id);
            query.append_pair("se", &se);
            query.append_pair("ep", &ep);
            if let Some(detail_path) = detail_path.as_deref() {
                query.append_pair("detailPath", detail_path);
            }
        }

        let request = FetchRequest::get(download_url)
            .with_header("Accept", "application/json")
            .with_header("X-Client-Info", r#"{"timezone":"UTC"}"#)
            .with_header("Referer", API_REFERER);
        let response = ctx.fetcher.request(request).await?;
        let payload: Value = serde_json::from_str(&response.body).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid API response: {e}"))
        })?;

        // `response.code !== 0 || !response.data?.downloads?.length` —
        // upstream returns [], a miss.
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let downloads = match payload
            .get("data")
            .and_then(|data| data.get("downloads"))
            .and_then(Value::as_array)
        {
            Some(list) if code == 0 && !list.is_empty() => list.as_slice(),
            _ => return Err(ExtractorError::NotFound),
        };

        let streams = downloads
            .iter()
            .map(|download| {
                let download = parse_download(download).ok_or_else(|| {
                    ExtractorError::extraction(self.id(), "download entry without a URL")
                })?;
                // `download.resolution || 0` — the label always renders.
                let resolution = download.resolution.unwrap_or(0);
                let stream_url = Url::parse(&download.url).map_err(|e| {
                    ExtractorError::extraction(self.id(), format!("invalid download URL: {e}"))
                })?;

                // `.m3u8` in the URL wins, then `.mp4` in the URL or the
                // declared format, else unknown.
                let format_upper = download
                    .format
                    .as_deref()
                    .map(str::to_uppercase)
                    .unwrap_or_default();
                let format = if stream_url.as_str().contains(".m3u8") {
                    Format::Hls
                } else if stream_url.as_str().contains(".mp4") || format_upper == "MP4" {
                    Format::Mp4
                } else {
                    Format::Unknown
                };

                // `parseInt(download.size, 10) || undefined`.
                let size = parse_size(download.size.as_deref()).filter(|bytes| *bytes > 0);

                let mut stream = Stream::new(stream_url, format)
                    .with_ttl(TTL)
                    .with_label(format!("{resolution}p"))
                    .with_referer(API_REFERER);
                stream.meta.languages = vec![CountryCode::Multi];
                if resolution > 0 {
                    stream.meta.resolution = u16::try_from(resolution).ok();
                }
                stream.meta.size = size;
                Ok(stream)
            })
            .collect::<Result<Vec<Stream>, ExtractorError>>()?;
        Ok(streams)
    }
}

/// `url.searchParams.get(name)` — the first value of `name`.
fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// One `downloads` entry: the URL is required, the rest is optional.
fn parse_download(value: &Value) -> Option<Download> {
    Some(Download {
        resolution: value.get("resolution").and_then(number_or_parsed_string),
        url: value.get("url")?.as_str()?.to_string(),
        format: value
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string),
        size: value
            .get("size")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// `parseInt(value, 10)` for a JSON number or numeric string.
fn number_or_parsed_string(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => parse_leading_u64(text),
        _ => None,
    }
}

/// `parseInt(text, 10)` — the leading ASCII digits of the size string.
fn parse_size(size: Option<&str>) -> Option<u64> {
    parse_leading_u64(size?)
}

/// `parseInt(text, 10)` — the digits at the start of `text`, after
/// `parseInt`'s leading-whitespace skip.
fn parse_leading_u64(text: &str) -> Option<u64> {
    let trimmed = text.trim_start();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    trimmed[..digits_end].parse().ok()
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://moviebox.com/watch?subjectId=123&se=1&ep=2")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_moviebox_family() {
        let extractor = MovieBox::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/watch?subjectId=1"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("moviebox.com"));
        assert!(supports("aoneroom.com"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn queries_the_download_api_for_every_download() {
        let fetcher = ScriptedFetcher::default().page(
            "/wefeed-h5api-bff/subject/download",
            r#"{"code":0,"data":{"downloads":[
                {"resolution":1080,"url":"https://dl.aoneroom.example/file/master.m3u8","format":"hls","size":"734003200"},
                {"resolution":720,"url":"https://dl.aoneroom.example/file/720.mp4","format":"MP4","size":"0"},
                {"url":"https://dl.aoneroom.example/file/other.mkv"}
            ]}}"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = MovieBox::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the download API must resolve: {e}"));
        assert_eq!(streams.len(), 3);

        assert_eq!(streams[0].label.as_deref(), Some("1080p"));
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(streams[0].meta.size, Some(734_003_200));

        assert_eq!(streams[1].label.as_deref(), Some("720p"));
        assert_eq!(streams[1].format, Format::Mp4);
        assert_eq!(streams[1].meta.resolution, Some(720));
        // `parseInt("0") || undefined` — a zero size is dropped.
        assert_eq!(streams[1].meta.size, None);

        // Missing resolution: the label still renders, no height.
        assert_eq!(streams[2].label.as_deref(), Some("0p"));
        assert_eq!(streams[2].format, Format::Unknown);
        assert_eq!(streams[2].meta.resolution, None);

        for stream in &streams {
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://videodownloader.site/")
            );
            assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        }

        // The API query carried the params and the upstream headers.
        let request = &fetcher.requests()[0];
        assert_eq!(
            request.url.as_str(),
            "https://h5-api.aoneroom.com/wefeed-h5api-bff/subject/download?subjectId=123&se=1&ep=2"
        );
        assert_eq!(
            request.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            request.headers.get("X-Client-Info").map(String::as_str),
            Some(r#"{"timezone":"UTC"}"#)
        );
        assert_eq!(
            request.headers.get("Referer").map(String::as_str),
            Some("https://videodownloader.site/")
        );
    }

    #[tokio::test]
    async fn passes_the_detail_path_when_present() {
        let fetcher = ScriptedFetcher::default().page(
            "/wefeed-h5api-bff/subject/download",
            r#"{"code":0,"data":{"downloads":[{"resolution":480,"url":"https://dl.aoneroom.example/file/480.mp4"}]}}"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://moviebox.com/watch?subjectId=9&detailPath=abc")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = MovieBox::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the download API must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let request = &fetcher.requests()[0];
        assert_eq!(
            request.url.as_str(),
            "https://h5-api.aoneroom.com/wefeed-h5api-bff/subject/download?subjectId=9&se=0&ep=0&detailPath=abc"
        );
    }

    #[tokio::test]
    async fn failed_responses_are_misses() {
        // `code !== 0`.
        let fetcher = ScriptedFetcher::default().page(
            "/wefeed-h5api-bff/subject/download",
            r#"{"code":1,"message":"gone"}"#,
        );
        let ctx = ctx_for(&fetcher, None);
        match MovieBox::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a failed response must be a NotFound, got {other:?}"),
        }

        // Empty downloads list.
        let fetcher = ScriptedFetcher::default().page(
            "/wefeed-h5api-bff/subject/download",
            r#"{"code":0,"data":{"downloads":[]}}"#,
        );
        let ctx = ctx_for(&fetcher, None);
        match MovieBox::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty download list must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn urls_without_a_subject_id_are_misses() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://moviebox.com/watch?se=1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match MovieBox::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a subjectless URL must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_api_json_is_a_scrape_failure() {
        let fetcher =
            ScriptedFetcher::default().page("/wefeed-h5api-bff/subject/download", "not json");
        let ctx = ctx_for(&fetcher, None);

        match MovieBox::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("invalid JSON must be a scrape failure, got {other:?}"),
        }
    }
}
