//! Persistent private JSON-lines worker for `scripts/provider_matrix.py`.
//! stdout contains signed stream URLs/headers and must remain a private pipe.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vsources::traits::{FetchRequest, FetchResponse, Fetcher, ProbeResponse, Source};
use vsources::{MediaId, MediaRef, MediaType, ResolveCtx, Stream};
use vsources_core::error::{FetchError, SourceError};
use vsources_core::mappings::MappingService;
use vsources_core::tmdb::TmdbClient;
use vsources_providers::CachedSource;

struct ObservedFetcher {
    inner: Arc<dyn Fetcher>,
    calls: AtomicUsize,
    contexts: Mutex<Vec<Value>>,
}
impl ObservedFetcher {
    fn observe(&self, request: &FetchRequest) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let host = request.url.host_str().unwrap_or_default();
        if [
            "api.themoviedb.org",
            "arm.haglund.dev",
            "graphql.anilist.co",
        ]
        .contains(&host)
        {
            return;
        }
        let headers: BTreeMap<_, _> = request
            .headers
            .iter()
            .filter(|(name, _)| {
                ["referer", "origin", "user-agent"].contains(&name.to_ascii_lowercase().as_str())
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let mut contexts = self
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if contexts.len() < 256 {
            contexts.push(json!({"url":request.url,"headers":headers}));
        }
    }
}
#[async_trait]
impl Fetcher for ObservedFetcher {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        self.observe(&request);
        self.inner.request(request).await
    }
    async fn probe(
        &self,
        request: FetchRequest,
        limit: usize,
    ) -> Result<Option<ProbeResponse>, FetchError> {
        self.observe(&request);
        self.inner.probe(request, limit).await
    }
}
#[derive(Deserialize)]
struct Request {
    case_id: String,
    tmdb: u64,
    kind: String,
    season: Option<u32>,
    episode: Option<u32>,
    #[serde(default)]
    english_dub: bool,
    #[serde(default = "source_budget")]
    timeout: u64,
    #[serde(default = "repeats")]
    warm_repeats: usize,
}
fn source_budget() -> u64 {
    35
}
fn repeats() -> usize {
    3
}

async fn timed_resolve(
    source: &dyn Source,
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    english: bool,
    timeout: u64,
    fetcher: &ObservedFetcher,
) -> (Value, Vec<Stream>) {
    let calls = fetcher.calls.load(Ordering::Relaxed);
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(timeout), async {
        if english {
            source.resolve_english_dub(ctx, media).await
        } else {
            source.resolve(ctx, media).await
        }
    })
    .await;
    let (status, cards, error) = match result {
        Ok(Ok(cards)) => (
            if cards.is_empty() {
                "empty"
            } else {
                "resolved"
            },
            cards,
            None,
        ),
        Ok(Err(SourceError::NotFound)) => ("empty", Vec::new(), None),
        Ok(Err(error)) => ("resolve_error", Vec::new(), Some(error.to_string())),
        Err(_) => ("resolve_timeout", Vec::new(), None),
    };
    (
        json!({"status":status,"ms":started.elapsed().as_secs_f64()*1000.0,
        "http_requests":fetcher.calls.load(Ordering::Relaxed)-calls,"cards":cards.len(),"error":error}),
        cards,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for file in [".env", ".env.local"] {
        let _ = dotenvy::from_filename(file);
    }
    let mut builder = vsources_net::ChromeFetcher::builder();
    if let Ok(proxy) = std::env::var("VSOURCES_PROXY") {
        builder = builder.proxy(proxy);
    }
    let inner: Arc<dyn Fetcher> = Arc::new(builder.build()?);
    let observed = Arc::new(ObservedFetcher {
        inner,
        calls: AtomicUsize::new(0),
        contexts: Mutex::new(Vec::new()),
    });
    let fetcher: Arc<dyn Fetcher> = observed.clone();
    let tmdb = Arc::new(
        TmdbClient::from_env(fetcher.clone()).ok_or("configure TMDB credentials in .env")?,
    );
    let mappings = MappingService::new(fetcher);
    let sources: Vec<_> = vsources_providers::wave1(tmdb.clone(), mappings.clone())
        .into_iter()
        .chain(vsources_providers::wave2(tmdb.clone(), mappings))
        .collect();
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--catalog") {
        let configured: Vec<_> = [
            "TMDB_API_KEY",
            "TMDB_ACCESS_TOKEN",
            "PECKLE_FEBBOX_COOKIE",
            "MOVIEBOX_MOBILE_SIGNING_KEY",
            "VSOURCES_PROXY",
        ]
        .into_iter()
        .filter(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
        .collect();
        println!(
            "{}",
            json!({"providers":sources.iter().map(|source| source.info()).collect::<Vec<_>>(),"configured_env":configured})
        );
        return Ok(());
    }
    let provider = args.get(1).ok_or("pass provider id or --catalog")?;
    let source = sources
        .into_iter()
        .find(|source| source.info().id == *provider)
        .ok_or("unknown provider")?;
    let base_url = source.info().base_url.clone();
    let source = CachedSource::new(source);
    for line in io::stdin().lock().lines() {
        let request: Request = serde_json::from_str(&line?)?;
        let output = audit_request(
            &observed,
            &tmdb,
            &source,
            provider,
            base_url.as_ref(),
            request,
        )
        .await;
        println!("{output}");
        io::stdout().flush()?;
    }
    Ok(())
}

async fn audit_request(
    observed: &ObservedFetcher,
    tmdb: &TmdbClient,
    source: &CachedSource,
    provider: &str,
    base_url: Option<&url::Url>,
    request: Request,
) -> Value {
    observed
        .contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let kind = if request.kind == "series" {
        MediaType::Series
    } else {
        MediaType::Movie
    };
    let media = MediaRef {
        id: MediaId::Tmdb(request.tmdb),
        kind,
        season: request.season,
        episode: request.episode,
    };
    let started = Instant::now();
    let metadata = tokio::time::timeout(Duration::from_secs(20), tmdb.resolve_media(&media)).await;
    let metadata_ms = started.elapsed().as_secs_f64() * 1000.0;
    if let Ok(Ok(meta)) = metadata {
        let title = meta.name.clone();
        let ctx = ResolveCtx {
            fetcher: observed,
            media: Some(meta),
            source_id: Some(provider),
            referer: None,
        };
        let (cold, streams) = timed_resolve(
            source,
            &ctx,
            &media,
            request.english_dub,
            request.timeout,
            observed,
        )
        .await;
        let mut warm = Vec::new();
        for _ in 0..request.warm_repeats {
            let budget = if streams.is_empty() && cold["status"] != "empty" {
                5
            } else {
                request.timeout
            };
            let (sample, _) =
                timed_resolve(source, &ctx, &media, request.english_dub, budget, observed).await;
            warm.push(sample);
            if cold["status"] != "resolved" && cold["status"] != "empty" {
                break;
            }
        }
        let contexts = observed
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({"provider":provider,"case_id":request.case_id,"title":title,"metadata_ms":metadata_ms,
                "cold":cold,"warm":warm,"streams":streams,"base_url":base_url,"observed_contexts":contexts})
    } else {
        json!({"provider":provider,"case_id":request.case_id,"metadata_ms":metadata_ms,
                "cold":{"status":"metadata_error","ms":0,"http_requests":0,"cards":0},"warm":[],"streams":[]})
    }
}
