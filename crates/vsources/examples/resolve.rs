//! Minimal resolve example for the `vsources` SDK.
//!
//! Run with a TMDB key:
//!
//! ```text
//! TMDB_API_KEY=... cargo run -p vsources --example resolve
//! ```

use vsources::{EngineBuilder, MediaId, MediaRef, MediaType};
use vsources_providers::SourceRegistry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Providers go here as waves land. Caching (15s empty / 5 min
    // results), NotFound-to-empty, and priority ordering come from the
    // registry wrapper; providers that resolve embeds take an
    // `Arc<ExtractorRegistry>` built over `hosts::all()`.
    let providers = SourceRegistry::new(vec![]);

    let engine = EngineBuilder::new().build()?;

    let media = MediaRef {
        id: MediaId::tmdb(27_205),
        kind: MediaType::Movie,
        season: None,
        episode: None,
    };

    let info = providers.list();
    println!("{} providers registered", info.len());
    for provider in engine.providers() {
        println!("provider: {} ({})", provider.label, provider.id);
    }

    let streams = engine.resolve(&media).await?;
    println!("{} streams", streams.len());
    Ok(())
}
