//! Live provider matrix, emitting one JSON record per completed provider.
//! Run with `TMDB_API_KEY` set; optional arguments are additional movie IDs.
//! `VSOURCES_AUDIT_PROVIDERS` restricts IDs (comma-separated).
//! This measures resolution and lightweight probes, not full playback.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde_json::json;
use vsources::liveness::{ProbeConfig, StreamProbe, Verdict};
use vsources::traits::Fetcher;
use vsources::{MediaId, MediaRef, MediaType, ResolveCtx};
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_net::ChromeFetcher;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fetcher: Arc<dyn Fetcher> = Arc::new(ChromeFetcher::builder().build()?);
    let tmdb = Arc::new(
        TmdbClient::from_env(fetcher.clone()).ok_or("set TMDB_API_KEY or TMDB_ACCESS_TOKEN")?,
    );
    let mappings = vsources_core::mappings::MappingService::new(fetcher.clone());
    let sources: Vec<_> = vsources_providers::wave1(tmdb.clone(), mappings.clone())
        .into_iter()
        .chain(vsources_providers::wave2(tmdb.clone(), mappings))
        .filter(|source| {
            std::env::var("VSOURCES_AUDIT_PROVIDERS")
                .map_or(true, |ids| ids.split(',').any(|id| id == source.info().id))
        })
        .collect();
    let probes = StreamProbe::new(ProbeConfig::default());
    let mut matrix = vec![
        (209_867, MediaType::Series),
        (37_854, MediaType::Series),
        (27_205, MediaType::Movie),
        (1396, MediaType::Series),
    ];
    for arg in std::env::args().skip(1) {
        matrix.push((arg.parse()?, MediaType::Movie));
    }
    for (id, kind) in matrix {
        let media = MediaRef {
            id: MediaId::Tmdb(id),
            kind,
            season: (kind == MediaType::Series).then_some(1),
            episode: (kind == MediaType::Series).then_some(1),
        };
        let metadata = tmdb.resolve_media(&media).await?;
        eprintln!(
            "Auditing {} ({id}), {} providers",
            metadata.name,
            sources.len()
        );
        let jobs = sources.iter().map(|source| {
            let fetcher = fetcher.as_ref(); let media = &media; let metadata = &metadata; let probes = &probes;
            async move {
                let started = Instant::now();
                let provider = &source.info().id;
                let ctx = ResolveCtx { fetcher, media: Some(metadata.clone()), source_id: Some(provider), referer: None };
                let result = tokio::time::timeout(Duration::from_secs(35), source.resolve(&ctx, media)).await;
                let resolve_ms = started.elapsed().as_millis();
                let (status, streams, error) = match result {
                    Ok(Ok(streams)) => (if streams.is_empty() { "empty" } else { "resolved" }, streams, None),
                    Ok(Err(SourceError::NotFound)) => ("empty", Vec::new(), None),
                    Ok(Err(error)) => ("error", Vec::new(), Some(error.to_string())),
                    Err(_) => ("timeout", Vec::new(), None),
                };
                let checks = streams.iter().map(|card| async move {
                    let verdict = probes.check(fetcher, card).await;
                    json!({"host": card.url.host_str(), "format": card.format, "external": card.is_external,
                        "verdict": match verdict { Verdict::Alive => "alive", Verdict::Dead => "dead", Verdict::Unknown => "unknown" }})
                });
                let cards: Vec<_> = stream::iter(checks).buffer_unordered(6).collect().await;
                json!({"tmdb": id, "title": metadata.name, "provider": provider, "status": status,
                    "resolve_ms": resolve_ms, "elapsed_ms": started.elapsed().as_millis(), "error": error,
                    "streams": cards})
            }
        });
        let mut results = stream::iter(jobs).buffer_unordered(6);
        while let Some(result) = results.next().await {
            println!("{result}");
        }
    }
    Ok(())
}
