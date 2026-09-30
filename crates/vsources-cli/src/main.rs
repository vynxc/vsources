//! Example CLI over the vsources SDK.
//!
//! `vsources providers` lists the registered providers, `vsources
//! resolve` fans a media reference out across them, `vsources extract`
//! routes an embed URL through the extractors, `vsources tt` parses
//! release names, and `vsources cf` drives `FlareSolverr`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use url::Url;
use vsources::types::Stream;
use vsources::{Engine, EngineBuilder, MediaId, MediaRef, MediaType};
use vsources_cloudflare::{FlareSolverr, SolveRequest};
use vsources_core::traits::{Fetcher, ResolveCtx};
use vsources_extractors::ExtractorRegistry;
use vsources_extractors::hosts;
use vsources_net::ChromeFetcher;

/// Resolve free streaming sources from the command line.
#[derive(Parser)]
#[command(name = "vsources", version, about)]
struct Cli {
    /// `FlareSolverr` daemon URL for Cloudflare-protected sites.
    #[arg(long, global = true)]
    flaresolverr: Option<Url>,

    /// HTTP proxy for scrape traffic.
    #[arg(long, global = true)]
    proxy: Option<String>,

    /// TMDB API key (defaults to `TMDB_API_KEY` / `TMDB_ACCESS_TOKEN`).
    #[arg(long, global = true)]
    tmdb_key: Option<String>,

    /// Per-provider resolve budget, in seconds.
    #[arg(long, global = true, default_value_t = 35)]
    timeout: u64,

    /// How many providers resolve concurrently.
    #[arg(long, global = true, default_value_t = 6)]
    concurrency: usize,

    /// Emit JSON where a table would otherwise be printed.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the registered providers.
    Providers,
    /// Resolve a movie or episode into direct streams.
    Resolve {
        /// Media id: `tmdb:27205`, `27205`, or `tt1375666`.
        id: String,
        /// Media kind.
        #[arg(long, value_enum, default_value_t = KindArg::Movie)]
        kind: KindArg,
        /// Season, for series.
        #[arg(long)]
        season: Option<u32>,
        /// Episode, for series.
        #[arg(long)]
        episode: Option<u32>,
        /// Restrict to these provider ids (repeatable).
        #[arg(long = "provider")]
        providers: Vec<String>,
    },
    /// Parse a release/torrent title and print structured metadata.
    Tt {
        /// The title to parse.
        title: String,
    },
    /// Extract direct streams from a player/embed URL.
    Extract {
        /// The embed URL.
        url: Url,
    },
    /// `FlareSolverr` / Cloudflare utilities.
    Cf {
        #[command(subcommand)]
        command: CfCommand,
    },
    /// Fetch one URL through the browser-impersonating fetcher and dump
    /// status, headers, and the body — the site-audit tool.
    Fetch {
        /// The URL to fetch.
        url: Url,
        /// Print only the first N bytes of the body.
        #[arg(long, default_value_t = 4000)]
        head: usize,
    },
}

#[derive(Subcommand)]
enum CfCommand {
    /// Check whether the `FlareSolverr` daemon answers.
    Health,
    /// Solve a Cloudflare-protected URL and print the clearance.
    Solve {
        /// The URL to clear.
        url: Url,
    },
}

/// The media kinds a reference can name.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum KindArg {
    /// A feature film.
    Movie,
    /// An episodic series.
    Series,
}

impl From<KindArg> for MediaType {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Movie => Self::Movie,
            KindArg::Series => Self::Series,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Providers => providers(cli),
        Command::Resolve {
            id,
            kind,
            season,
            episode,
            providers,
        } => resolve(cli, id, *kind, *season, *episode, providers).await,
        Command::Tt { title } => tt(title),
        Command::Extract { url } => extract(cli, url).await,
        Command::Fetch { url, head } => fetch_url(cli, url, *head).await,
        Command::Cf { command } => cf(cli, command).await,
    }
}

/// Whether a TMDB key is available via flag or environment.
fn tmdb_configured(cli: &Cli) -> bool {
    cli.tmdb_key.is_some()
        || std::env::var_os("TMDB_API_KEY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("TMDB_ACCESS_TOKEN").is_some_and(|v| !v.is_empty())
}

/// The engine with the global flags applied.
fn build_engine(cli: &Cli, provider_ids: &[String]) -> Result<Engine, vsources::EngineError> {
    let mut builder = EngineBuilder::new()
        .per_source_timeout(Duration::from_secs(cli.timeout))
        .concurrency(cli.concurrency);
    if let Some(url) = &cli.flaresolverr {
        builder = builder.flaresolverr(url.clone());
    }
    if let Some(proxy) = &cli.proxy {
        builder = builder.proxy(proxy.clone());
    }
    if let Some(key) = &cli.tmdb_key {
        builder = builder.tmdb_key(key.clone());
    }
    if tmdb_configured(cli) {
        // The default catalog needs TMDB for title/id metadata; without
        // a key the engine resolves nothing and says so.
        builder = builder.with_default_providers();
    }
    if !provider_ids.is_empty() {
        let ids: Vec<&str> = provider_ids.iter().map(String::as_str).collect();
        builder = builder.providers(&ids);
    }
    builder.build()
}

fn providers(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let engine = build_engine(cli, &[])?;
    let infos = engine.providers();
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&infos)?);
    } else if infos.is_empty() {
        println!("no providers registered");
    } else {
        println!("{:<16} {:<24} {:>8}  BASE URL", "ID", "LABEL", "PRIORITY");
        for info in infos {
            println!(
                "{:<16} {:<24} {:>8}  {}",
                info.id,
                info.label,
                info.priority,
                info.base_url
                    .as_ref()
                    .map_or_else(|| "-".to_string(), ToString::to_string)
            );
        }
    }
    Ok(())
}

async fn resolve(
    cli: &Cli,
    id: &str,
    kind: KindArg,
    season: Option<u32>,
    episode: Option<u32>,
    provider_ids: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let media_id = MediaId::parse(id)
        .ok_or_else(|| format!("unrecognized media id `{id}` (want tmdb:27205 or tt1375666)"))?;
    let media = MediaRef {
        id: media_id,
        kind: kind.into(),
        season,
        episode,
    };
    let engine = build_engine(cli, provider_ids)?;
    let streams = engine.resolve(&media).await?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&streams)?);
    } else if streams.is_empty() {
        println!("no streams found for {id}");
    } else {
        for (index, stream) in streams.iter().enumerate() {
            print_stream(index + 1, stream);
        }
    }
    Ok(())
}

fn tt(title: &str) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = vsources_tt::parse_torrent_title(title);
    println!("{}", serde_json::to_string_pretty(&parsed)?);
    Ok(())
}

async fn extract(cli: &Cli, url: &Url) -> Result<(), Box<dyn std::error::Error>> {
    // Reuse the engine's fetcher so `--flaresolverr` and `--proxy` apply.
    let engine = build_engine(cli, &[])?;
    let fetcher = engine.fetcher().clone();
    let registry = ExtractorRegistry::new(hosts::all());
    let ctx = ResolveCtx {
        fetcher: fetcher.as_ref(),
        media: None,
        source_id: Some("cli"),
        referer: None,
    };
    let streams = registry.extract(&ctx, url).await?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&streams)?);
    } else if streams.is_empty() {
        println!("no streams extracted from {url}");
    } else {
        for (index, stream) in streams.iter().enumerate() {
            print_stream(index + 1, stream);
        }
    }
    Ok(())
}

/// Fetch one URL through the browser-impersonating fetcher and dump
/// the response — the site-audit tool.
async fn fetch_url(cli: &Cli, url: &Url, head: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ChromeFetcher::builder();
    if let Some(proxy) = &cli.proxy {
        builder = builder.proxy(proxy.clone());
    }
    let fetcher = builder
        .build()
        .map_err(|error| format!("fetcher build failed: {error}"))?;
    let response = fetcher
        .request(vsources_core::traits::FetchRequest::get(url.clone()))
        .await
        .map_err(|error| format!("fetch failed: {error}"))?;
    println!("status: {}", response.status);
    println!("final url: {}", response.url);
    for (name, value) in &response.headers {
        println!("{name}: {value}");
    }
    println!();
    let body = &response.body;
    let cut = body
        .char_indices()
        .nth(head)
        .map_or(body.len(), |(index, _)| index);
    println!("{}", &body[..cut]);
    Ok(())
}

async fn cf(cli: &Cli, command: &CfCommand) -> Result<(), Box<dyn std::error::Error>> {
    let fetcher: Arc<dyn Fetcher> = Arc::new(
        ChromeFetcher::builder()
            .build()
            .map_err(|error| format!("fetcher build failed: {error}"))?,
    );
    let client = match &cli.flaresolverr {
        Some(url) => FlareSolverr::new(url.clone(), Arc::clone(&fetcher)),
        None => FlareSolverr::from_env(Arc::clone(&fetcher))
            .ok_or("no FlareSolverr configured: pass `--flaresolverr` or set `FLARESOLVERR_URL`")?,
    };

    match command {
        CfCommand::Health => {
            if client.is_available().await {
                let version = client.version().await.unwrap_or_default();
                if version.is_empty() {
                    println!("FlareSolverr is up");
                } else {
                    println!("FlareSolverr is up ({version})");
                }
                Ok(())
            } else {
                Err("FlareSolverr is not answering".into())
            }
        }
        CfCommand::Solve { url } => {
            let request = SolveRequest::get(url.as_str()).with_max_timeout(60_000);
            let response = client.solve(&request).await?;
            let solution = response
                .solution
                .as_ref()
                .ok_or("solve returned no solution")?;
            println!("status:    {}", response.status);
            println!("user agent: {}", solution.user_agent);
            println!("final url: {}", solution.url);
            match solution
                .cookies
                .iter()
                .find(|cookie| cookie.name == "cf_clearance")
            {
                Some(clearance) => println!("cf_clearance: {}", clearance.value),
                None => println!("cf_clearance: (none returned)"),
            }
            Ok(())
        }
    }
}

/// One human-readable stream line.
fn print_stream(index: usize, stream: &Stream) {
    let quality = match (stream.meta.resolution, stream.meta.quality.as_deref()) {
        (Some(height), Some(quality)) => format!("{height}p {quality}"),
        (Some(height), None) => format!("{height}p"),
        (None, Some(quality)) => quality.to_string(),
        (None, None) => "-".to_string(),
    };
    let label = stream
        .label
        .as_deref()
        .map_or(String::new(), |label| format!(" \"{label}\""));
    let external = if stream.is_external {
        " [external]"
    } else {
        ""
    };
    // Audio/subtitle markers: `[dub]`/`[sub]` when the meta knows.
    let track = match (stream.meta.dubbed, stream.meta.subbed) {
        (Some(true), Some(true)) => " [dub|sub]",
        (Some(true), _) => " [dub]",
        (_, Some(true)) => " [sub]",
        _ => "",
    };
    println!(
        "{index:>3}. {} ({:?}) {quality}{track}{label}{external}",
        stream.url, stream.format
    );
}
