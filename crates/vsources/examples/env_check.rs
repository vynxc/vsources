//! Headless end-to-end checks using `.env`/environment credentials.
//!
//! Default: full-catalog Inception resolve. `--series`: Cinemeta episode
//! picker identity → Breaking Bad S2E1 resolve. `--anikage-playback`:
//! Reincarnated as a Sword S2E1 through `AniKage` → three decoded mpv frames.

use std::time::{Duration, Instant};
use vsources::{EngineBuilder, MediaId, MediaRef, MediaType, Stream};

// This check reuses the TUI's wire parser; interactive search state is unused.
#[allow(dead_code)]
mod cinemeta;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for name in [".env", ".env.local"] {
        let _ = dotenvy::from_filename(name);
    }
    let args: Vec<_> = std::env::args().collect();
    let playback = args.iter().any(|arg| arg == "--anikage-playback");
    let series = args.iter().any(|arg| arg == "--series");
    let mut builder = EngineBuilder::new().with_default_providers();
    if playback {
        builder = builder.providers(&["anikage"]);
    }
    if series {
        builder = builder.providers(&["vidlink", "vidzee", "cineby"]);
    }
    let engine = builder.build()?;
    println!("{} providers registered", engine.providers().len());
    let media = if playback {
        MediaRef::series(MediaId::tmdb(134_667), 2, 1)
    } else if series {
        let imdb = "tt0903747";
        let episodes = cinemeta::fetch_episodes(engine.fetcher().as_ref(), imdb).await?;
        let episode = episodes
            .iter()
            .find(|ep| ep.season == 2 && ep.episode == 1)
            .ok_or("Cinemeta omitted S2E1")?;
        println!(
            "{} episodes; selected S{:02}E{:02}: {} ({})",
            episodes.len(),
            episode.season,
            episode.episode,
            episode.title,
            episode.released.as_deref().unwrap_or("no date")
        );
        MediaRef::series(MediaId::Imdb(imdb.into()), episode.season, episode.episode)
    } else {
        MediaRef {
            id: MediaId::tmdb(27_205),
            kind: MediaType::Movie,
            season: None,
            episode: None,
        }
    };
    let start = Instant::now();
    let streams = engine.resolve(&media).await?;
    println!(
        "{} streams in {:.1}s",
        streams.len(),
        start.elapsed().as_secs_f32()
    );
    for stream in streams.iter().take(5) {
        println!(
            "  {:>4}p  {}  {}",
            stream.meta.resolution.unwrap_or(0),
            stream.meta.source_id.as_deref().unwrap_or("?"),
            stream.label.as_deref().unwrap_or("")
        );
    }
    if streams.is_empty() {
        return Err("no streams resolved".into());
    }
    if playback {
        let mut played = false;
        for stream in streams {
            let label = stream.label.clone().unwrap_or_default();
            let decoded = tokio::task::spawn_blocking(move || decode_frames(&stream)).await?;
            println!("frame check {label}: {decoded}");
            if decoded {
                played = true;
                break;
            }
        }
        if !played {
            return Err("mpv did not decode three frames".into());
        }
        println!("mpv decoded three frames with the stream's required headers");
    }
    Ok(())
}

fn decode_frames(stream: &Stream) -> bool {
    let mut command = std::process::Command::new("mpv");
    command.args([
        "--no-config",
        "--network-timeout=12",
        "--frames=3",
        "--vo=null",
        "--ao=null",
        "--audio=no",
        "--terminal=yes",
        "--term-playing-msg=video-decoder-ready",
        "--term-status-msg=decoded-frame=${estimated-frame-number}",
    ]);
    for (name, value) in &stream.meta.request_headers {
        command.arg(format!("--http-header-fields-append={name}: {value}"));
    }
    if stream.format == vsources::types::Format::Hls {
        command.arg("--demuxer-lavf-o=allowed_extensions=ALL,allowed_segment_extensions=ALL,extension_picky=0,protocol_whitelist=[http,https,tcp,tls,crypto,data]");
    }
    // Output stays private: mpv can include signed URLs in errors.
    command.arg(stream.url.as_str());
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(45) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return false;
    };
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "mpv exit={:?}, decoder_ready={}, video_output={}",
        output.status.code(),
        log.contains("video-decoder-ready"),
        log.contains("VO: [null]")
    );
    output.status.success() && log.contains("video-decoder-ready") && log.contains("VO: [null]")
}
