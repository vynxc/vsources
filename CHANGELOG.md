# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fast English-dub anime — 2026-09-29

- Added native AniWaves and EchoVideo source extraction, with strict media
  matching, bounded catalog caching, and a dub-only path that skips SUB servers.
- Added `Source::resolve_english_dub`, `Engine::resolve_english_dub`, and the
  12-second `Engine::resolve_fast_english_dub` race across AniWaves, ReAnime
  and AnimeKai. CLI: `resolve ... --english-dub --fast`.
- Isolated English-dub provider results and misses from ordinary resolve caches.
- Added required `StreamMeta::audio_selection` for embedded multi-audio files.
  ReAnime verifies English Matroska audio tracks from a bounded 128 KiB prefix;
  files without explicit English track metadata do not qualify. Normal cards
  describe multi-audio instead of assuming their default audio is dubbed.
- Forwarded selected audio to the example mpv launcher. Recorded live source
  research and native playback evidence under `docs/audits/`.

### Playback verification — 2026-09-26

- Added an FFmpeg playback audit: 41 of 47 registered SDK provider routes
  decoded eight seconds of video and audio. MovieBox requires a runtime
  signing key; six providers remain unverified. Evidence and reproduction:
  `docs/audits/2026-09-26-playback.md`.
- Fixed false Cloudflare challenge detection on healthy passive-JS pages
  and ordinary rate limits, plus a Unicode title-parser panic.
- Added binary request bodies and bounded binary API responses; implemented
  CineJoy's sealed POST and preserved IMDBPlay's WASM bytes.
- Updated ReAnime, ZXCStream, VidZee and AniKage for current protocols;
  corrected AnimeZeY matching, string sizes and signed download origins.
- Preserved required Nexabloom/Video CDN headers and opaque HLS format hints;
  accepted PlayIMDB's string status code; removed unbounded provider-local
  media validation from AniKage, AnimeZeY and NikaStream.
- Added MovieBox's optional mobile DASH adapter with injected signing key,
  anonymous visitor sessions and signed CDN cookies. Filtered its legacy
  upgrade-notice clip.
- Fixed TUI/mpv forwarding of multiple playback headers and HLS media served
  under unconventional segment extensions.

### Quality sweep — 2026-09-26

- Added bounded binary `Fetcher::probe` and shared engine liveness checks;
  inconclusive probes remain eligible, and dead verdicts expire after 30 s.
- Implemented the missing VidKing media fallback and generic EmbedResolver.
  Shared the existing speedracelight implementation without changing its
  provider import path; fallback requests coalesce per media identity.
- Isolated extractor caches by media/episode and Referer context.
- Avoided allocating unused progressive snapshots in ordinary `resolve`.
- Consolidated AnimeKai/AniKage scripted test fetchers with header and Range
  response support.
- Added the reusable live audit example and recorded 329 broad cases plus
  49 fallback reruns. Corrected the completion claim: four extractor stubs
  remain. See `docs/audits/2026-09-26.md`.

### Added

- `vsources-tt`: full parse-torrent-title port — release names to
  structured metadata, with the upstream handler corpus as tests.
- `vsources-core`: domain types, `Fetcher`/`Source`/`Extractor` traits,
  TMDB client with caches, domain resolver with mirror racing and
  dead-domain tracking, `p,a,c,k,e,d` unpacker, release-name enrichment.
- `vsources-cloudflare`: challenge detection, FlareSolverr client, host
  cooldowns, extensible solver chain.
- `vsources-net`: `ChromeFetcher` — browser emulation, per-host
  queueing, timeouts, redirect handling, Cloudflare retries.
- `vsources-extractors`: host extractors behind a caching,
  coalescing registry (`hosts::all()`), including the hub-cloud family,
  the speedracelight fallback, and hand-ported AES-CBC/GCM, ChaCha20,
  and RC4 schemes where the hosts need them.
- `vsources-providers`: provider infrastructure — `CachedSource`
  (5 min results bounded by stream TTLs, 15 s negative window,
  in-flight coalescing, NotFound-to-empty), `SourceRegistry`
  (priority ordering), and the wave-1 catalog: 24 English providers
  (`wave1`).
- `vsources --example tui`: a ratatui/crossterm terminal UI over the
  SDK — query form (id, movie/series, season, episode), backgrounded
  resolve with a spinner, a quality-sorted stream table with dub/sub
  track tags, and `mpv` playback of the selection (including the
  hotlink headers a player must send). The engine is built lazily so a
  missing TMDB key is a status-line hint instead of a startup crash.
  Results are progressive: the table fills as providers answer (live
  "N streams so far" status), re-sorted as batches land, with the
  selection pinned to its stream across re-sorts.
- `vsources`: `Engine::resolve_progressive` — the same merge, dedup,
  enrichment, and ordering as `resolve`, but the merged stream list is
  sent through a channel as each provider completes; the final snapshot
  is the return value. A late answer from a higher-priority provider
  takes over a URL an earlier arrival claimed, matching the single-shot
  winner.
- `vsources`: the `Engine` facade — bounded fan-out, per-source 35 s
  budgets, URL dedup, enrichment, quality sort — plus
  `EngineBuilder` (flaresolverr, proxy, TMDB key/client, provider
  allowlist, `with_default_providers`).
- `vsources-cli`: example CLI — `providers`, `resolve`, `tt`,
  `extract`, `cf health|solve`.
- `README.md` with architecture, embedding notes (Tauri/Android), and
  the provider wave table.

### Nuvio plumbing

- `vsources-providers::nuvio`: the shared `nuvioHelpers.js` layer the
  wave-2 providers build on — JS-shaped stream deserialization,
  `buildStreamResults` (header policy, no-referer gates, force-HLS
  hints), filename cleaning, height/size parsing, audio-track labels.
- `nuvio::speedracelight`: the seed/provider fetch stack (25 s seed
  cache, 120 s edge-5xx down-mark, in-flight coalescing, 401
  invalidate-and-retry) and the custom-PRNG "mvm1" payload cipher —
  sparse-table hole semantics verified against the upstream JS as an
  oracle.
- `nuvio::vidstorm`: AES-256-GCM token decryption with the bundle key
  derivation, ground-truth verified against Node's `crypto`.
- `nuvio::flixcloud`: the WASM key-mixing decrypt (rotation chain
  corrected against the live module's bytecode).
- `nuvio::megaplay`: the RC4-ish blob decrypt.
- `nuvio::decrypt`: image-disguise detection, PBKDF2-SHA256, and the
  XOR helpers shared by the Nuvio decryptors.

### Provider waves

- Wave 1 (24 self-contained scrapers): AllWish, AniBD, AniDoor,
  AniKage, Anikoto, AnimeFlix, AnimeGG, AnimeKai, HiAnime, Itachi,
  TwoDhive, CineWave, IMDBPlay, MovieBox, Necro, Netlio, NowHDTime,
  Peckle, PrimeShows, VidFast, VidKing, VidSrcSbs, VidZee, WatchSeries.
- Waves 2a/2b (23 Nuvio-backed providers, `wave2`): anime — AniChan,
  AnikotoTV, AnimeSuge, AnimeZeY, AniMoTVSlash, NikaStream, ReAnime,
  StreamXTV; movies/TV — AcerMovies, Atlantic, Cineby, CinebyRocks,
  CineJoyAllInOne, FrameX, PlayImdb, Raflix, RiveStream, Stellar,
  VidEasy, VideasyTo, VidLink, VixSrc, ZXCStream.

### Added

- `vsources-cli fetch <url>`: dump any URL through the
  browser-impersonating fetcher — status, headers, and body — the
  site-audit tool for triaging dead links and Cloudflare gating.

### Fixed

- AnimeKai no longer ships zoko's fake DUB rows: zoko's `/dub`
  endpoint silently mirrors the sub file when a dub is missing
  (live-verified on Frieren E1 — byte-identical segments — against
  One Piece E1, which differs). A 1-byte `Content-Range` probe on the
  first segment of each audio category detects the mirror and drops
  the mislabeled rows; an inconclusive probe keeps both.
- AniKage no longer ships cards for the dead anicore relay: a
  liveness GET on the first card (with the Origin/Referer a player
  sends, now also attached to every AniKage stream) answers
  `NotFound` during relay outages instead of shipping URLs the player
  cannot resolve.
- `Engine` provider outcomes are processed in completion order again
  (`buffer_unordered`): the boxed-futures refactor had switched the
  fan-out to `buffered`, which yields in registry order — one slow
  high-priority provider could hold back every later provider's
  results.

- `MediaId::parse` now accepts the `tmdb:27205` prefix form the CLI
  documents (it previously rejected even its own example; bare `27205`
  and `tt1375666` forms were unaffected). `parse_media_ref` handles the
  same prefix with `:season:episode` parts, and both carry regression
  tests.
- `StreamMeta` carries explicit `dubbed`/`subbed` booleans, filled by
  the release-name enrichment from the torrent-title parser's markers
  (`DUB`, `SUB`, `DUAL AUDIO`, …) — a provider's own answer always
  wins. The CLI prints them as `[dub]`/`[sub]` tags and they ride in
  the `--json` output.
- `EngineBuilder::with_default_providers` now assembles all 47
  providers (wave 1 + wave 2) over the shared TMDB client; the
  CLI's `providers` command lists the full catalog.
