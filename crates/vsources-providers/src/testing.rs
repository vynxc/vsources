//! Test doubles shared by the provider crate's unit tests.

use std::collections::BTreeMap;
use std::sync::Mutex;
use url::Url;

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::helpers::direct_stream;

/// A fetcher that never performs I/O.
pub(crate) struct NoopFetcher;

#[async_trait]
impl Fetcher for NoopFetcher {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        Err(FetchError::Transport {
            url: request.url,
            message: "noop fetcher".to_string(),
        })
    }
}

/// A resolve context over the [`NoopFetcher`].
pub(crate) fn stub_ctx() -> ResolveCtx<'static> {
    static FETCHER: NoopFetcher = NoopFetcher;
    ResolveCtx {
        fetcher: &FETCHER,
        media: None,
        source_id: None,
        referer: None,
    }
}

/// A series episode reference.
pub(crate) fn stub_media() -> MediaRef {
    MediaRef {
        id: MediaId::Tmdb(1396),
        kind: MediaType::Series,
        season: Some(1),
        episode: Some(2),
    }
}

/// A minimal descriptor for tests.
pub(crate) fn stub_info(id: &str, priority: i32) -> SourceInfo {
    SourceInfo {
        id: id.to_string(),
        label: id.to_string(),
        content_types: vec![MediaType::Movie, MediaType::Series],
        country_codes: vec![CountryCode::En],
        base_url: None,
        priority,
        domain_key: None,
    }
}

/// What a [`CountingSource`] answers.
pub(crate) enum Outcome {
    /// `SourceError::NotFound`.
    NotFound,
    /// An empty success.
    Empty,
    /// One direct stream.
    OneStream,
    /// A scrape failure.
    Error,
}

/// A [`Source`] that counts calls and replays a fixed outcome.
pub(crate) struct CountingSource {
    /// The descriptor served by `info`.
    pub info: SourceInfo,
    /// How many `resolve` calls reached this source.
    pub calls: AtomicUsize,
    /// The outcome to replay.
    pub outcome: Outcome,
}

#[async_trait]
impl Source for CountingSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        _ctx: &ResolveCtx<'_>,
        _media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.outcome {
            Outcome::NotFound => Err(SourceError::NotFound),
            Outcome::Empty => Ok(Vec::new()),
            Outcome::OneStream => Ok(vec![direct_stream(
                url::Url::parse("https://cdn.example/v.mp4")
                    .unwrap_or_else(|_| panic!("invalid test URL")),
                Format::Mp4,
                std::time::Duration::from_secs(300),
                &url::Url::parse("https://origin.example/")
                    .unwrap_or_else(|_| panic!("invalid test URL")),
            )]),
            Outcome::Error => Err(SourceError::scrape("counter", "boom")),
        }
    }
}

/// A canned-body matcher: `(host, path)`.
type PageRule = Box<dyn Fn(&Url) -> bool + Send + Sync>;
/// One scripted page: the matcher, body, and extra response headers.
type ScriptedPage = (PageRule, String, Vec<(String, String)>);

/// A fetcher that serves canned bodies (plus optional response
/// headers, for the Range probe) keyed by a URL matcher, and
/// records every request it sees.
#[derive(Default)]
pub(crate) struct ScriptedFetcher {
    pages: Mutex<Vec<ScriptedPage>>,
    requests: Mutex<Vec<FetchRequest>>,
}

impl ScriptedFetcher {
    /// Serve `body` to every request whose URL matches `matches`.
    pub(crate) fn page<F>(self, matches: F, body: impl Into<String>) -> Self
    where
        F: Fn(&Url) -> bool + Send + Sync + 'static,
    {
        self.push_page(matches, body.into(), Vec::new())
    }

    /// Serve a 1-byte Range answer whose `Content-Range` total is
    /// `total` — the mirror probe's segment endpoint.
    pub(crate) fn ranged<F>(self, matches: F, total: u64) -> Self
    where
        F: Fn(&Url) -> bool + Send + Sync + 'static,
    {
        self.push_page(
            matches,
            "x".to_string(),
            vec![("content-range".to_string(), format!("bytes 0-0/{total}"))],
        )
    }

    /// The shared page push.
    fn push_page<F>(self, matches: F, body: String, headers: Vec<(String, String)>) -> Self
    where
        F: Fn(&Url) -> bool + Send + Sync + 'static,
    {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((Box::new(matches), body, headers));
        self
    }

    /// Every request seen so far, in order.
    pub(crate) fn requests(&self) -> Vec<FetchRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The value of a header on the first request to `path`.
    pub(crate) fn header_sent_to(&self, path: &str, name: &str) -> Option<String> {
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

#[async_trait]
impl Fetcher for ScriptedFetcher {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let url = request.url.clone();
        let page = self
            .pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(matches, _, _)| matches(&url))
            .map(|(_, body, headers)| (body.clone(), headers.clone()));
        match page {
            Some((body, extra)) => {
                let mut headers =
                    BTreeMap::from([("content-type".to_string(), "text/html".to_string())]);
                headers.extend(extra);
                Ok(FetchResponse {
                    url,
                    status: 200,
                    headers,
                    body,
                })
            }
            None => Err(FetchError::NotFound { url }),
        }
    }
}

impl ScriptedFetcher {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn page_url(self, key: impl Into<String>, body: impl Into<String>) -> Self {
        let key = key.into();
        self.page(move |url| key_of(url) == key, body)
    }
    pub(crate) fn remove_url(&self, key: &str) {
        let url = Url::parse(&format!("https://{key}")).unwrap_or_else(|e| panic!("test URL: {e}"));
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(matches, _, _)| !matches(&url));
    }
    pub(crate) fn sent_header(&self, key: &str, name: &str) -> Option<String> {
        self.requests()
            .iter()
            .find(|r| key_of(&r.url) == key)
            .and_then(|r| {
                r.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            })
    }
}
pub(crate) fn key_of(url: &Url) -> String {
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    format!(
        "{}{}{query}",
        url.host_str().unwrap_or_default(),
        url.path()
    )
}
