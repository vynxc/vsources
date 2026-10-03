# Provider playback matrix

The case manifest contains five movies, five TV series, five anime series and
five anime movies, mixing classics with recent releases and later seasons.
Every registered provider attempts every case. The original run had 48 × 20 =
960 combinations; after retiring AniBD and AnimeZeY, the catalog has 46 × 20 =
920 current combinations. Their original evidence remains in history.
The default anime policy calls the SDK's English-dub resolver. `--no-english-dub`
accepts ordinary sub/dub results in a separate output directory.

## Run and view

From the repository root with TMDB credentials in `.env`, `.env.local`, or the
process environment, and `cargo` and FFmpeg on PATH:

```sh
python3 scripts/provider_matrix.py --output docs/audits/2026-10-02-matrix
python3 -m http.server 8766 --bind 127.0.0.1 --directory docs/audits/2026-10-02-matrix
```

Open `http://127.0.0.1:8766/index.html`. The page updates every 15 seconds during
the run. `index.html` also opens offline with its embedded snapshot; it needs no
CDN, external fonts, server, npm package, or account. Filter by provider/title,
category or result. Select a matrix cell for all its recorded stream and decode
attempts; switch graph metrics between source resolution, first decoded frame,
and the combined latency estimate. JSON includes every result and prior attempt;
CSV exports the currently filtered table.

The dashboard uses Tabler Core 1.6.1 with Overview, Results, and Coverage matrix
tabs, plus the GitHub Repair log. The latency graph defaults to the fastest 12 providers; choose All providers
for the full comparison. Click a table row or matrix cell to open playback evidence
in the side panel. Reset filters clears search, category, result, and latency metric.
The MIT-licensed UI assets are vendored in `scripts/vendor/tabler` and embedded
in each generated HTML report, including reports viewed offline.

The runner builds its worker before testing and freezes that exact executable
for the run. `--no-build` deliberately uses an existing binary. Every result
records its worker SHA-256; repaired-provider reruns retain older results in
`history`. A complete snapshot contains 960 current results, not just successes.

## Resume and repeat

```sh
# Continue incomplete work without repeating completed cases.
python3 scripts/provider_matrix.py --resume --output docs/audits/2026-10-02-matrix

# Rebuild and retest selected providers, retaining all other current results.
python3 scripts/provider_matrix.py --resume --rerun anikage,animekai \
  --output docs/audits/2026-10-02-matrix

# Regenerate HTML/summary/environment from saved data, with no provider calls.
python3 scripts/provider_matrix.py --report-only --output docs/audits/2026-10-02-matrix

# A small isolated run; use a separate output and generated-env destination.
python3 scripts/provider_matrix.py --providers anikage,reanime \
  --output /tmp/vsources-small-matrix --generated-env /tmp/vsources-small.env

# Local regression checks (including a real header-gated HTTP media decode).
python3 -m unittest discover -s scripts -p 'test_provider_matrix.py'
```

Use a new output directory when changing the case manifest or audio policy.
`--workers`, `--source-timeout`, `--decode-timeout`, `--seconds`, `--cards`,
`--warm-repeats`, `--header-retries` and `--no-warm-playback` control cost/budgets.
Defaults: six provider workers, 35-second source budgets, 40-second decode
budgets, eight-second playback, three immediate warm source calls, up to two
SDK cards and three diagnostic header profiles per failed card. Alternate SDK
cards are tried before header experiments. A passing card ends that case's
candidate walk; unsampled cards remain unverified.

## What the timings prove

Each provider has a persistent SDK process and the standard `CachedSource`.
The first request for each title is followed immediately by three repeats in
the same instance. First-call timings exclude the separately recorded metadata
lookup. Upstream clients may already be warm from preceding titles. Counts are
Fetcher request/probe calls, not transport-level redirects or packet counts.
Warm zero-request results distinguish memory reuse from real upstream retries;
empty results have a separate, short negative cache. In English-dub mode,
empty can mean no qualifying English card, even when ordinary sub streams exist.

Playback uses FFmpeg with every SDK request header, explicit selected audio
indices, and restricted network protocols. HLS accepts valid video segments
whose filenames end in `.jpg`. A pass requires exit zero, actual video frames,
near-full requested duration and nonzero decoded audio. First-frame latency is
the first positive FFmpeg progress sample, with a 0.1-second reporting period.
Repeat playback launches a fresh FFmpeg process with the same selected cached
URL. It measures repeated startup, not persistent player memory. Host decode
queues are serialized and recorded separately.

Combined first-frame estimates sum metadata, source resolution, preceding failed
candidate attempts, and successful decode startup. They are composite estimates,
not a timing measured in a device player. Short playback proves neither
whole-title throughput nor complete episode/content identity. Language tags,
SDK category metadata and required embedded-track selection are recorded;
speech is not independently transcribed. A selected non-English audio tag on
an English-required case is marked `wrong_audio`.

Known anime-only routes returning media for regular movie/TV requests are
marked `wrong_catalog`; their transport evidence remains visible. The Casablanca
case exposed an anime/title collision with the classic film. This conservative
scope gate prevents those routes from becoming movie recommendations.

## Environment generation and private evidence

`.env.generated` is written only when every selected provider/case is complete.
It contains a global provider allowlist and category shortlist hints, with no
credentials. Coverage comes first; reliable coverage ties prefer faster combined
startup. Cold SDK playback, valid repeated playback (when measured), matching
catalog scope and acceptable selected language are required. A header experiment
alone never qualifies: repair the SDK and rerun the provider first. Uncovered
titles remain listed instead of inventing coverage. An empty shortlist disables
resolution with the unmatched `__none__` sentinel.

```sh
set -a
source .env
source .env.generated
set +a
cargo run -p vsources-cli -- providers --json
```

`EngineBuilder::with_default_providers()` reads `VSOURCES_PROVIDERS` unless the
caller explicitly uses `.providers(...)`; `.providers(&[])` explicitly selects
all. `VSOURCES_AUDIT_CASES` supplies the runner's case-manifest default. Category
`VSOURCES_AUDIT_*_PROVIDERS` values are advisory lists for callers choosing a
category; the engine does not infer anime categories automatically. The matrix
worker always inventories and tests its requested catalog regardless of the
application allowlist. Existing `.env` credentials are preserved. The generated global list also retains
verified `AniWaves`/`ReAnime`/`AnimeKai` candidates (ranked by observed startup)
for the SDK's existing fast-dub race. Known non-animation TMDB metadata keeps
anime-only routes out of ordinary movie/TV requests; this uses the already
fetched genres, adds no request, and preserves eligibility when genres are unknown.

Public `report.json`, `results.jsonl` and HTML omit signed URLs, header values,
cookies, raw error messages and raw API bodies. Private response snapshots,
process logs and header-recovery details live under the runner's `--private`
directory (default `/tmp/vsources-provider-matrix-20261002`, mode 0700). Future
runs use unique log subdirectories, keeping repeated attempts separate. The first
repair run reused some original private filenames; public before/after metrics
and decode evidence are retained in history. Archived worker binaries preserve
both tested implementations. Credential-requiring providers remain unverified
when the caller has not configured their own session/key.
