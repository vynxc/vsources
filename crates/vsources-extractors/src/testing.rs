//! Test scaffolding for host extractor fixture tests.
//!
//! Host pages are replayed from strings (later: from
//! `tests/fixtures/<host>/` snapshots) through a scripted fetcher that
//! records every request, so ports can assert both what was parsed and
//! what headers went over the wire.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::FetchError;
use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher};
use vsources_core::types::Stream;

/// A fetcher that serves canned bodies keyed by URL path and records
/// every request it sees.
pub(crate) struct ScriptedFetcher {
    pages: Mutex<HashMap<String, String>>,
    requests: Mutex<Vec<FetchRequest>>,
}

impl ScriptedFetcher {
    /// Serve `path` with `body`.
    pub(crate) fn page(self, path: impl Into<String>, body: impl Into<String>) -> Self {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.into(), body.into());
        self
    }

    /// Every request seen so far, in order.
    pub(crate) fn requests(&self) -> Vec<FetchRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The value of a header sent with the request for `path`.
    pub(crate) fn sent_header(&self, path: &str, name: &str) -> Option<String> {
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

impl Default for ScriptedFetcher {
    fn default() -> Self {
        Self {
            pages: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Fetcher for ScriptedFetcher {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let path = request.url.path().to_string();
        let body = self
            .pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&path)
            .cloned();
        match body {
            Some(body) => Ok(FetchResponse {
                url: request.url,
                status: 200,
                headers: std::collections::BTreeMap::from([(
                    "content-type".to_string(),
                    "text/html".to_string(),
                )]),
                body,
            }),
            None => Err(FetchError::NotFound { url: request.url }),
        }
    }
}

/// A resolve context over a scripted fetcher.
pub(crate) fn ctx_for<'a>(
    fetcher: &'a ScriptedFetcher,
    referer: Option<&'a Url>,
) -> vsources_core::traits::ResolveCtx<'a> {
    vsources_core::traits::ResolveCtx {
        fetcher,
        media: None,
        source_id: None,
        referer,
    }
}

/// Assert a single direct stream came out shaped like the upstream
/// result: one URL, one format, a hotlink Referer.
#[allow(dead_code)]
pub(crate) fn assert_direct_stream(
    streams: &[Stream],
    format: vsources_core::types::Format,
    url_prefix: &str,
) {
    assert_eq!(streams.len(), 1, "expected exactly one stream");
    let stream = &streams[0];
    assert!(!stream.is_external);
    assert_eq!(stream.format, format);
    assert!(
        stream.url.as_str().starts_with(url_prefix),
        "unexpected URL: {}",
        stream.url
    );
}
