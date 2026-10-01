//! Resolve the first English-dub anime source, then demonstrate warm cache reuse.
//!
//! `TMDB_API_KEY=... cargo run -p vsources --example fast_dub -- 209867 1 2`
//! Prints timings and provider/audio identity only; never prints media URLs.

use std::time::Instant;

use vsources::{EngineBuilder, MediaId, MediaRef};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let id = args.first().map_or(Ok(209_867), |id| id.parse::<u64>())?;
    let season = args.get(1).map_or(Ok(1), |season| season.parse::<u32>())?;
    let episode = args
        .get(2)
        .map_or(Ok(2), |episode| episode.parse::<u32>())?;
    let engine = EngineBuilder::new().with_default_providers().build()?;
    let media = MediaRef::series(MediaId::Tmdb(id), season, episode);
    for run in 1..=3 {
        let started = Instant::now();
        let Some(stream) = engine.resolve_fast_english_dub(&media).await? else {
            return Err("No direct English-dub source found".into());
        };
        println!(
            "{}",
            serde_json::json!({
                "run":run,
                "resolve_ms":started.elapsed().as_secs_f64()*1000.0,
                "provider":stream.meta.source_id,
                "audio_selection":stream.meta.audio_selection,
            })
        );
    }
    Ok(())
}
