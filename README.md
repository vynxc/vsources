# vsources

A Rust SDK for resolving free streaming sources — movies, series, and anime —
as a library and a CLI. No HTTP server, no addon protocol: this is the
scraping stack from
[ignatiusphoenix](https://github.com/SaugatXthaa/ignatiusphoenix) (itself the
PhoeniX lineage) ported to native Rust, designed to be embedded in a server, a
Tauri desktop app, or an Android client.

What is inside:

- **`vsources-tt`** — a full port of
  [parse-torrent-title](https://github.com/cjd05/parse-torrent-title): release
  names (`The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1`) to structured
  metadata (title, year, resolution, quality, codec, audio, languages, …).
- **`vsources-core`** — the domain types (`Stream`, `MediaRef`, `SourceInfo`),
  the `Fetcher`/`Source`/`Extractor` traits, a TMDB client with caching, a
  domain resolver (env overrides, mirror racing, dead-domain tracking), the
  `p,a,c,k,e,d` unpacker, and release-name enrichment.
- **`vsources-cloudflare`** — Cloudflare challenge detection, a
  [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr) client, host
  cooldowns, and a solver chain you can extend with custom solvers.
- **`vsources-net`** — a browser-emulating fetcher (`ChromeFetcher`) with
  per-host queueing, timeouts, redirect handling, and the Cloudflare chain
  wired in.
- **`vsources-extractors`** — embed/player extractors (dood, filemoon,
  megaplay, vidzee, the hub-cloud family, the speedracelight fallback, …)
  behind a caching, coalescing registry.
- **`vsources-providers`** — the English-only provider catalog (waves below),
  with per-provider result caching (5 min), negative caching (15 s), and
  priority ordering.
- **`vsources`** — the facade `Engine`: bounded fan-out over providers, per
  source 35 s budget, URL dedup, enrichment, quality sort — plus
  `resolve_progressive`, which streams the merged list as each provider
  completes instead of waiting for the fleet.
- **`vsources-cli`** — an example CLI exercising everything.
- **`vsources --example tui`** — a ratatui TUI: search a title on Cinemeta,
  select a movie or pick a series episode by season, title and air date,
  then play the selected stream in `mpv` (hotlink headers passed through).
  Results are progressive — streams land in the quality-sorted table as
  providers answer (first results in seconds), re-sorted as they arrive,
  with the selection pinned across re-sorts.

## Quick start

```console
$ cargo run -p vsources-cli -- tt 'The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1'
{
  "title": "The Matrix",
  "year": "1999",
  "resolution": "1080p",
  "quality": "BluRay",
  "codec": "x264",
  "audio": ["DTS Lossless"],
  "channels": ["5.1"],
  ...
}

$ cargo run -p vsources-cli -- providers
$ TMDB_API_KEY=... cargo run -p vsources-cli -- resolve tmdb:27205 --kind movie --json
$ cargo run -p vsources-cli -- extract 'https://filemoon.to/e/abc123'
$ cargo run -p vsources-cli -- --flaresolverr http://localhost:8191/ cf solve 'https://example.com/'

$ TMDB_API_KEY=... cargo run -p vsources --example tui
```

The TUI reads `.env` and `.env.local`. Search and episode metadata need no key;
resolution uses TMDB credentials. `Tab` moves between the query and kind fields,
`m`/`s` selects movies/series, and `Enter` searches and chooses a result. Series
open a scrollable episode list; select an episode with `↑↓` and `Enter` to
resolve it. `Esc` returns to the series results. Movies resolve directly.

All anime providers in the default catalog share one cached ARM/AniList mapping
service. Providers with AniList/MAL endpoints resolve the requested season by ID.
Sites exposing only internal catalog keys try that season's canonical title,
then their existing title matching. Custom provider instances can opt in with
`.with_mappings(mapping_service.clone())`; AniKage has the associated constructor
`AniKage::with_mappings(mapping_service)`.

Global flags: `--flaresolverr URL`, `--proxy URL`, `--tmdb-key KEY`,
`--timeout SECONDS`, `--concurrency N`, `--json`.

Provider matrix (2026-10-02): **48 providers × 20 titles = 960 current checks**,
plus 320 before/after repair attempts. The [interactive report](docs/audits/2026-10-02-matrix/index.html)
shows cold/cached resolve latency, startup/decode timing, headers, audio selection,
and every failed or empty outcome. [Runner and methodology](docs/audits/provider-matrix.md).

```sh
python3 scripts/provider_matrix.py --resume --output docs/audits/2026-10-02-matrix
# Retest repaired providers while preserving the full report/history:
python3 scripts/provider_matrix.py --resume --rerun anikage,animekai --output docs/audits/2026-10-02-matrix
```

The completed audit generates `.env.generated` without credentials. Load it after
`.env` to use `VSOURCES_PROVIDERS`; explicit `.providers(...)` overrides that
selection. Known non-animation metadata keeps anime-only routes out of regular
film/TV resolutions. Genre classification reuses the existing TMDB details request.

GitHub fast-provider research (2026-10-02), outside the SDK's current catalog:

| Candidate | Fresh URL resolution | First decoded frame after URL | Decision |
|---|---:|---:|---|
| Castle TV | 0.999 s | 0.731 s | Strongest new API lead; English-tagged Inception played |
| VaPlayer | 0.291 s | 2.838 s | Movie/TV lead; anime dub coverage is incomplete |
| FibWatch | 2.046 s | 0.710 s | Fast playback; select the file's embedded English track |
| VidRock | 0.223 s | about 6 s | Fast resolver, slower startup; outside the strict shortlist |
| Kurage | 2.269–3.941 s, with a timeout | 2.686 s | Dub playback works, latency too variable for fast-only use |

Each accepted sample decoded eight seconds of video/audio. These are private
protocol prototypes, **not five new integrated SDK providers**. Three candidates
passed the fresh shortlist screen; five verified fast replacements were not
established. [Pinned GitHub sources, timing evidence and rejected leads](docs/audits/2026-10-02-fast-provider-research.json).

Historical playback audit (2026-09-26): **41/47 registered provider routes decoded
eight seconds of video and audio**. Forty passed with TMDB configuration;
MovieBox also needed `MOVIEBOX_MOBILE_SIGNING_KEY`. AcerMovies, IMDBPlay,
NowHDTime, Peckle, Stellar and VixSrc remain unverified. See the
[playback evidence and blockers](docs/audits/2026-09-26-playback.md).

Provider-specific configuration:

- `MOVIEBOX_MOBILE_SIGNING_KEY`: hex signing key for the current mobile DASH
  API. The default catalog reads it from the environment; custom providers
  can call `MovieBox::with_mobile_signing_key(Vec<u8>)`. No key is bundled.
  Without it, MovieBox uses its legacy web flow, which did not pass this audit.
- `PECKLE_FEBBOX_COOKIE`: the caller's own FebBox cookie containing `ui=...`.
  Peckle returns no results when this required cookie is absent.

Players must forward every `meta.request_headers` entry, including signed
cookies where present. MovieBox DASH manifests currently use `Format::Unknown`;
players should also inspect the URL or response format.

## The library

Fast English-dub anime resolution:

```sh
TMDB_API_KEY=... cargo run -p vsources-cli -- resolve tmdb:209867 \
  --kind series --season 1 --episode 2 --english-dub --fast --json
```

`Engine::resolve_fast_english_dub` races AniWaves, ReAnime and AnimeKai and
returns the first direct dub within a 12-second total budget. It respects the
configured provider allowlist, cancels unfinished work, and reuses per-provider
five-minute caches without mixing SUB and DUB results. `resolve_english_dub`
returns the complete filtered list instead. Normal `resolve` is unchanged.

ReAnime's progressive files can default to Japanese. English resolution reads
at most 128 KiB of Matroska track metadata and sets
`meta.audio_selection = { language: "En", audio_index: 1 }` when the second
audio stream is explicitly tagged English. The actual index is inspected, not
assumed. Players **must honor that selection**: FFmpeg uses `-map 0:a:N`;
native players should select the corresponding audio track. The example mpv
launcher forwards the selection. A custom `Fetcher` needs a bounded binary
`probe` implementation for this verification; missing or incomplete metadata
cannot qualify as an English dub.

AniWaves is a new native provider using the current EchoVideo API. Later seasons
require explicit matching season titles; uncertain title/year/type matches are
rejected. See the [source research](docs/audits/2026-09-29-anime-dub-speed.json)
and [native implementation verification](docs/audits/2026-09-29-anime-dub-native.json)
for measured startup, audio checks, and limitations. First resolution completion
is not a guarantee of fastest player startup or highest resolution.

```rust
use vsources::{EngineBuilder, MediaId, MediaRef, MediaType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = EngineBuilder::new()
        .with_default_providers()
        // .flaresolverr(url::Url::parse("http://localhost:8191/")?)
        .build()?;

    let media = MediaRef {
        id: MediaId::tmdb(27_205),
        kind: MediaType::Movie,
        season: None,
        episode: None,
    };
    let streams = engine.resolve(&media).await?;
    Ok(())
}
```

Register your own providers with `EngineBuilder::sources(vec![...])`, or
restrict resolution with `.providers(&["cineby", "vidlink2"])`. Every stream
carries `meta.request_headers` when the host gates hotlinked media on
`Referer`/`User-Agent` — there is no server-side proxy hop to hide behind, so
any player (Tauri `reqwest`, Android OkHttp, an Axum route) applies them
directly.

## Media validation

The engine checks direct streams before returning them or emitting snapshots.
HLS probes follow up to three playlist levels and sample the first segment;
direct-file probes request a 2 KiB prefix so archive and media signatures can
be distinguished. The default fetcher caps reads even if Range is ignored.
HTML pages, archives, images and missing files are filtered. Network failures,
Cloudflare/IP blocks, rate limits and unsupported formats remain inconclusive.

`vsources::liveness::ProbeConfig` controls the shared concurrency (6), total
chain/provider-batch budget (4 s), and verdict lifetimes (alive 5 min, dead
30 s, unknown 5 s, bounded by stream TTL). Pass it to
`EngineBuilder::probe_config`; set `enabled: false` to opt out. Unfinished
probes keep their streams. External pages and known single-use Google
Downloads are skipped. Embedders using a custom fetcher can implement the
optional `Fetcher::probe` method; its default performs no I/O and returns
inconclusive. `StreamProbe` is also available for standalone checks.

Coverage is still incomplete: 37 host modules, 33 registry entries, and four
remaining stubs (`nuvio`, `vidsrcme`, `anipriv8`, `zxcstream`). The restored
VidKing and generic embed fallbacks are implemented. See the
[earlier liveness audit](docs/audits/2026-09-26.md) for provider-policy gaps and
the [playback audit](docs/audits/2026-09-26-playback.md) for actual decoding.

## Cloudflare

`ChromeFetcher` detects challenge pages automatically. Give the engine a
FlareSolverr daemon and challenges are solved and retried transparently:

```rust
EngineBuilder::new()
    .flaresolverr(url::Url::parse("http://localhost:8191/")?)
    .build()?
```

Custom solvers slot into `vsources_cloudflare::SolverChain` — implement
`CloudflareSolver` (for example, a future local-browser solver) and build a
`ChromeFetcher` with `.cloudflare(chain)` yourself instead of using the
engine's default fetcher.

## Provider catalog

English-only by policy: English-primary and anime (English-subbed) providers
are kept; Hindi-first and other non-English providers are cut. The cut is
trivially reversible — one module plus one registry entry.

| Wave | Providers |
|------|-----------|
| 1 — self-contained scrapers (25) | AniWaves, AllWish, AniBD, AniDoor, AniKage, Anikoto, AnimeFlix, AnimeGG, AnimeKai, HiAnime, Itachi, TwoDhive, CineWave, IMDBPlay, MovieBox, Necro, Netlio, NowHDTime, Peckle, PrimeShows, VidFast, VidKing, VidSrcSbs, Vidzee, WatchSeries |
| 2a — Nuvio-backed anime (8) | AniChan, AnikotoTV, AnimeSuge, AnimeZeY, AniMoTVSlash, NikaStream, ReAnime, StreamXTV |
| 2b — Nuvio-backed movies/TV (15) | AcerMovies, Atlantic, Cineby, CinebyRocks, CineJoyAllInOne, FrameX, PlayImdb, Raflix, RiveStream, Stellar, VidEasy, VideasyTo, VidLink, VixSrc, ZXCStream |

The three provider waves are registered: `vsources_providers::wave1` assembles the
self-contained scrapers and `vsources_providers::wave2` the 23
Nuvio-backed providers (the engine assembles both via
`EngineBuilder::with_default_providers()` — 48 providers total). The
extractor registry resolves embeds through `vsources_extractors::hosts::all()`; the Nuvio VidKing-family providers share a speedracelight seed store.
The registry’s VidKing fallback additionally coalesces results by media.
## Architecture

```
vsources-tt ──► vsources-core ◄── vsources-cloudflare
                    ▲   ▲
                    │   └────────► vsources-net (ChromeFetcher)
                    │                  ▲
vsources-extractors ┤                  │
        ▲            │                  │
        └── vsources-providers ◄────────┘
                 ▲
             vsources (Engine) ──► vsources-cli
```

- Per-provider caching lives in `vsources-providers::CachedSource`
  (5 min results bounded by stream TTLs, 15 s negative window,
  in-flight coalescing). Not-found is an answer, not an error: only a
  failure of *every* selected provider surfaces from `Engine::resolve`.
- Domain mirror fallbacks, `{KEY}_BASE_URL` env overrides, and dead-domain
  tracking live in `vsources-core::domain::DomainResolver`.
- Tests are offline: every scraper module pins its fixtures inline
  (HTML/JSON shaped from the upstream flows) behind a scripted mock
  `Fetcher`. A live smoke is a CLI resolve with a real TMDB key.

## Embedding notes

- **Tauri / desktop**: depend on `vsources`, build the `Engine` once, call
  `resolve` from a command handler. Streams with `is_external` are pages, not
  media; direct results carry `meta.request_headers`. Definitively dead media is filtered,
  while inconclusive probes are kept for the player to try.
- **Android (Kotlin)**: expose the engine through an FFI surface or run it
  behind a local service; the crate has no global state beyond the caches, so
  one engine per process is the intended shape.
- **Server**: mount the engine in any framework; `Engine::providers()` gives
  the catalog and `resolve` the merged, sorted stream list.

## Development

```console
$ cargo test --workspace --all-targets
$ cargo test --workspace --doc
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --workspace
```

The workspace denies `unsafe`, `unwrap`/`expect`, and missing docs.
