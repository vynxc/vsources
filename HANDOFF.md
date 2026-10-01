# Handoff — English-dub source research (updated 2026-09-30)

Workspace: `/mnt/ALPH/code/vsources`; `/home/vynxc/code/vsources` resolves here.
An initial commit now exists (`934de2d`). Preserve the working tree and new
audit/source files; do not reset or remove untracked files as cleanup.

## Current priority: large native/API catalogs; integration deferred

The latest user instruction is to find more fast, stable API-based providers,
especially mobile app backends, and finish integration **later**. They explicitly
prioritize large catalogs and rejected Tubi. Keep Tubi and other limited AVOD
catalogs outside the recommendation shortlist. No UI or provider implementation
was added in this follow-up research batch; the earlier SDK work is preserved.

Evidence: [mobile/API research](docs/audits/2026-09-30-mobile-api-research.json).

- The strongest new **catalog protocol** lead is ShowBox/SuperStream:
  `mbpapi.shegu.net/api/api_client/index/` accepts anonymous encrypted `Search5`
  requests. It found strict title/type/year entries for all five standard
  samples in roughly 0.15–0.3 seconds each. Native IDs are recorded in the audit.
  This is catalog evidence, not verified English-dub playback or a measured
  whole-library count. The current primary implementation uses public
  ShowBox share-link JSON and FebBox file-list JSON, then `/file/player` JSON
  with the caller's own FebBox `ui` cookie. No such session is available.
  Legacy download modules returned upgrade-required errors; the older anonymous
  `/hls/main/{oss_fid}.m3u8` route returned HTTP 200 with a non-playlist body.
  Never borrow a cookie or count these responses as playable success.
- MovieBlast is a real Android-style JSON API and returned four English-labelled
  Inception qualities, but their `move26.mbaccess.site` host failed DNS resolution.
  The other four reference lookups gave no cards. DooFlix's app API host was also
  unusable. Filmix search/session APIs responded, but selected video-link requests
  returned blocked-video 403s. Loklok's mobile searches/home feed were empty and
  its current H5 signing flow returned `B0001` errors. Do not call these permanent
  outages or validated replacements.
- KAA has a direct JSON episode API. Its reference Python code erroneously builds
  Japanese episode maps for dub requests; correct `lang=en-US` gives different
  slugs and verified English episode metadata. Actual media segments returned
  403, so it did not earn a playback recommendation. Kurage exposes a tRPC API
  and played real English Frieren/JJK, but missed Demon Slayer, varied in latency,
  and uses AnimeGG upstream. It is an aggregator interface, not independent
  storage or a new all-in-one catalog. See corrected `servers` response parsing
  and wider metadata-only coverage in the audit.
- Further direct Rive backend checks: Apex/AsiaCloud returned `data:null`.
  PrimeVids returned HLS for all five and four short samples decoded, but every
  sampled anime's dialogue was Japanese; Inception failed and Breaking Bad
  startup was about 26 seconds. It does not meet the requested dub/speed criteria.
- **Fresh end-of-batch baselines still passed 5/5 for Castle and 5/5 for MovieBox.**
  Castle now returns `img1.hlnom.com` / `img1.klnwm.com`, rather than the earlier
  hscow/fcxmb hosts. Follow current API-returned URLs and inspect actual audio
  tracks for each file. Persist exact title/season/episode identifiers; signed
  URLs and file-specific audio indices need expiry/file-change revalidation.
  MovieBox's fresh baseline explicitly used 480p. These are separated spot
  checks, not continuous uptime measurements or new production integration.

No newly verified large all-in-one replacement emerged in this batch. Castle and
MovieBox remain the demonstrated API leaders. ShowBox/FebBox is the concrete
conditional integration lead for later, pending caller session/playback checks.
Private probes and response/log artifacts are in
`/tmp/vsources-mobile-api-20260930` (0700); the repo audit excludes credentials,
signed URLs, share keys, raw bodies, WAVs and transcripts.

## Latest research: stable all-in-one sources

The user's latest request was sustained research for additional stable providers
covering movies, regular TV and actual English-dub anime. The VIPTV migration
remains **paused**. This research changed only documentation/audit evidence;
Castle and the other new candidates are **not integrated into the SDK**.

Evidence: [stability research](docs/audits/2026-09-30-stability-research.json),
following the [earlier all-in-one research](docs/audits/2026-09-30-all-in-one-research.json).

- **Castle TV / `api.hlowb.com` is the strongest new result.** Its anonymous
  native HTTP/AES JSON flow covers all five sampled titles: Inception,
  Breaking Bad S1E1, Frieren / Demon Slayer / Jujutsu Kaisen S1E2. All five
  passed 60-second video and selected English-audio decodes. Corrected burst
  checks passed 15/15. Six spaced rounds gave 29/30 label-filtered successes;
  the single miss was separately recovered from its exact returned file by
  selecting embedded English. Median API resolution was about 1.1 seconds and
  first decoded frame after receiving the URL about 0.34 seconds. Speech
  samples detect English, including anime >0.97
  confidence; Inception needed a later dialogue sample after sparse earlier
  speech. No account cookie or Cloudflare solver was needed on this connection.
- Important extractor corrections: do not discard English tracks when another
  language has `existIndividualVideo=true`; do not discard a shared file merely
  because its card is labelled Hindi/Japanese. Inspect actual muxed/HLS language
  tags and select English. Required audio indices in sampled files were
  Inception 0, Breaking Bad 1, Frieren 0, Demon Slayer 1, JJK 3. These indices
  are file-specific, not constants for future episodes. URL deduplication must
  retain audio selection metadata. A spaced label-only lookup missed Demon
  Slayer; the exact returned file contained English and played after selecting
  index 1. The audit preserves both the miss and its correction.
- Castle and Citadel use the **same CDN family**, `img1.hscow.com` /
  `img1.fcxmb.com`. Alternative APIs do not provide independent media hosting.
  Castle is a useful direct resolver; MovieBox remains an independent-CDN backup.
- **MovieBox passed 30/30 fresh short checks across six rounds.** Original
  unrefreshed URLs also passed 5/5 at about 69 minutes. Highest-quality 60-second
  transfers were often slower than real time, including a Breaking Bad timeout.
  Selecting the actual 480p representation passed all five 60-second tests in
  about 21–36 seconds. Do not interpret short playback success as throughput.
- Castle signed HLS URLs advertise **three-hour `Expires` timestamps**. Older
  Citadel URLs on this same CDN played after about 60–80 minutes. Cache exact
  URLs, required headers and selected English audio together until the earliest
  expiry minus a margin; re-resolve the exact media/provider on failure. Full
  three-hour longevity, permanent validity and persistent SDK caching are not
  proven/implemented. Persist strict provider media/season/episode/language IDs
  until identity failure requires revalidation.
- CinemaOS's current frontend uses `/api/providerv6/scrape`, not the obsolete
  `/api/cinemaosv2` GitHub references. V6 JSON decrypts and separates English dub
  MP4/DASH variants, but sampled media proxies were inconsistent; not a primary
  stability recommendation. Net27's anonymous variants API likewise exposes
  English dub, but every selected media sample returned HTTP 426.
- VaPlayer played all five titles, but Frieren/Demon Slayer dialogue was Japanese.
  Movix/Purstream played Frieren with French audio. Neither qualifies as complete
  English-dub coverage. Additional provider screens and blockers are in the audit;
  do not classify blocked or outdated adapters permanently dead.

Protocol references are pinned in the JSON report, including readable Castle
JavaScript and current Castle Kotlin implementations. Production work must add
strict title/type/year/season gates: the reference JavaScript falls back to its
first search row, which is not acceptable. Keep long numeric IDs as strings.
Context7 did not index this Castle API; live calls and primary extractor sources
provided the evidence.

Private bounded playback logs, short speech samples and research prototypes are
under `/tmp/vsources-overnight-20260930` (0700). The repo audit excludes tokens,
signing keys, cookies, signed media URLs, response bodies, WAVs and transcripts.
The SDK implementation and 827-test validation below predate this research.

## Latest task and implemented result

The VIPTV migration plan is **paused**. The user requested the fastest real
English-dub anime sources, then authorized implementing the best improvements.
Only this SDK repository changed; no VIPTV migration or new UI was added.

- Catalog: **48 providers**, 37 host modules, 33 registered extractors.
- New native `AniWaves` provider and `EchoVideo` extractor. Title/year/type gates,
  explicit later-season matching, five-minute identity caching, and a dub-only
  extraction path. No unrelated VidKing/media fallback. The protocol reference
  is aryaniiil/anime-api `aniwaves.py` at `594d12ec`.
- Added `Source::resolve_english_dub`, `Engine::resolve_english_dub` and
  `Engine::resolve_fast_english_dub`. The fast path races AniWaves, ReAnime and
  AnimeKai within the configured allowlist; 12-second total budget, first direct
  qualifying result, unfinished futures dropped. First resolution completion is
  not a measured player-startup comparison or a maximum-quality guarantee.
- `CachedSource` keeps normal and English-dub results/misses under separate keys.
  Existing result TTL limits remain; no indefinite or persistent cache added.
- ReAnime English resolution reads at most 128 KiB of the progressive file's
  Matroska header. `vsources-core::audio::matroska_english_audio_index` parses
  Tracks/TrackEntry and explicit Language/LanguageIETF tags, excluding subtitles.
  Missing/incomplete/unsupported metadata does not qualify as an English dub.
- New `StreamMeta::audio_selection: Option<AudioSelection>` carries required
  spoken language and zero-based audio index. Players must honor it and must
  not silently fall back to the default Japanese track. The example mpv launcher
  converts to its one-based audio ID; live mpv confirmed English audio ID 2.
- Normal progressive ReAnime labels now describe multi-audio. English-selected
  cards clearly identify English DUB; repeated access IDs are resolved once.
- CLI: `resolve ... --english-dub --fast`. `--fast` requires `--english-dub`.
  `--provider reanime --english-dub` gives the verified multi-audio 1080p route.
- `scripts/verify_playback.py` now honors required embedded audio selection and
  supports the English-dub/fast flags. `examples/fast_dub.rs` demonstrates one
  engine resolving the same episode three times, reporting only safe timings.

Evidence: [source research](docs/audits/2026-09-29-anime-dub-speed.json) and
[final native verification](docs/audits/2026-09-29-anime-dub-native.json).
Three episodes: Frieren, Demon Slayer and Jujutsu Kaisen S1E2. AniWaves, ReAnime
with English index 1, and the fast race each passed (9/9 native eight-second
video/audio decodes). Sampled dialogue was detected as English with >0.97
confidence through ReAnime and the fast route. Prior research includes nine
trials per leading provider and 60-second decodes for both AniWaves/ReAnime.

Final fast race: median resolve **1.399 s**, first decoded frame after URL
**0.693 s**, combined **2.114 s** on this connection. Warm engine resolves:
**12.56 / 13.18 ms**. These are short samples and immediate cache reuse, not
five-minute longevity, full-title, device-rendering or universal speed proofs.
Tested AniWaves output is 720p; tested ReAnime is 1080p.

**827 tests pass** (821 across all targets + 6 doctests). Formatting, Clippy
and rustdoc with warnings denied pass. Tests include strict matching, binary
track parsing/truncation, SUB skipping, required audio selection, cache isolation
and early return without waiting for slow sources. Logs are
`/tmp/vsources-anime-{tests,doc-tests,clippy,doc}.log`.

Live research/prototypes/private logs and short speech samples reside under
`/tmp/vsources-anime-speed-20260929`, mode 0700. Real TMDB/signing/session values
must not be copied into the repository. JSON audits exclude signed media URLs,
headers, cookies, raw speech and transcripts. Source graph MCP is available but
this repository is not indexed; file search was the necessary fallback.

The remaining notes below describe the earlier audit baseline; their 47-provider
coverage and test counts are historical, not a fresh full-catalog verification.

## User's active objective and honest status

Verify at least one media that **plays** from every provider. Use GitHub code
search for missing/current extractors when necessary.

**41/47 registered SDK provider routes passed an eight-second video-and-audio
decode. The objective is not fully complete.** Forty passed with ordinary TMDB
configuration; MovieBox also needed an injected mobile signing key. Six remain
unverified: `acermovies`, `imdbplay`, `nowhdtime`, `peckle`, `stellar`, `vixsrc`.

Start with [the playback report](docs/audits/2026-09-26-playback.md) and
[consolidated evidence](docs/audits/2026-09-26-playback-results.json).
Every successful sample includes requested TMDB identity, frames, duration,
audio confirmation, timings, codec summaries, timestamp and CLI binary hash.
These are short decoding proofs, not whole-title or content-identity reviews.
Several routes share a VidKing/other fallback; do not call them 41 independent
upstream backends.

## Project shape

Rust SDK, CLI and ratatui TUI for English movies/TV/anime source resolution,
ported from ignatiusphoenix without its Stremio HTTP server. Keep the facade
embeddable in servers, Tauri and Android.

Crates: title parser → core → Cloudflare/network/extractors → providers →
engine → CLI. The catalog has 24 self-contained and 23 Nuvio-backed providers.
There are 36 host modules, 32 registered extractors and four generic extractor
stubs: `nuvio`, `vidsrcme`, `anipriv8`, `zxcstream`. A working provider-specific
inline route does not mean the same-named generic extractor is implemented.
VidKing and EmbedResolver were implemented in the preceding quality sweep.

Upstream reference: `/tmp/ignat-tree/ignatiusphoenix`, especially `src/source`,
`src/extractor`, and `src/utils/streamGate.cjs`. Repository:
https://github.com/SaugatXthaa/ignatiusphoenix.
GitHub CLI is authenticated and read-only code search worked. Current protocol
references are linked in the playback report. Do not embed keys from examples.

## Repairs in the playback pass

- Cloudflare detection no longer treats passive JSD script references on
  HTTP 200 pages or ordinary HTTP 429 as host-wide challenges. Explicit
  managed-challenge markers still trigger the solver path.
- `FetchRequest` supports `binary_body` and `post_bytes`. The network layer
  sends raw bytes. CineJoy's sealed POST was a literal stub; it now sends
  binary requests and reads bounded encrypted responses. IMDBPlay's WASM
  now uses raw bounded bytes, fixing lossy text corruption.
- ReAnime uses the current Svelte devalue `/d/{accessId}/__data.json`
  metadata and signed download route before its legacy HLS fallback.
- ZXCStream has a current API adapter in `zxcstream/current.rs`, including
  OpenSSL/CryptoJS salted AES-CBC and MD5 key derivation. It tries the
  current API within eight seconds before the legacy Byse path.
- VidZee uses the current `core.vidzee.wtf/streams/...` API and preserves
  its required `https://player.vidzee.wtf/` Referer, with a coalesced cache.
- AniKage discovers `PUBLIC_PROXY_URL` from the homepage, caches it for
  five minutes, and falls back to `prox.anikage.cc`. `prox.anicore.tv` is
  obsolete. Both Origin and Referer matter.
- AnimeZeY accepts string/numeric sizes, handles release-group prefixes and
  bare episode 01, and keeps download signatures on the issuing worker.
- Nuvio cards retain Nexabloom's Referer and explicit HLS format hints for
  opaque URLs. Atlantic emits that HLS hint after playlist confirmation.
- Title-parser match offsets are rebased when tags are removed, fixing an
  emoji/Unicode slicing panic that broke VidEasy/VideasyTo results.
- PlayIMDB accepts string `"200"` as well as numeric status codes.
- AnimeZeY, AniKage and NikaStream leave final media validation to the
  bounded engine gate instead of downloading bodies in their providers.
- MovieBox's optional `moviebox/mobile.rs` adapter signs requests with a
  caller-injected key, obtains anonymous visitor sessions and preserves
  signed CDN cookies for DASH. No key is bundled. The default catalog reads
  hex `MOVIEBOX_MOBILE_SIGNING_KEY`; custom instances opt in with
  `with_mobile_signing_key(Vec<u8>)`. Its legacy upgrade-notice clip is
  filtered. DASH is currently represented as `Format::Unknown`.
- TUI/mpv uses `--http-header-fields-append` for each header and configures
  HLS demuxing to accept real MPEG-TS segments named `.jpg`, while limiting
  protocols. Players must preserve all stream headers, including cookies.

## Six remaining blockers and next work

| Provider | Observed evidence | Useful next action |
|---|---|---|
| AcerMovies | Current `api2.acermovies.fun` search/qualities work; source calls return only `fromCache:false`. A returned Modpro link is parked. Multiple sampled titles fail. | Recheck the source service/links; changing the API hostname alone does not help. |
| IMDBPlay | WASM decryption now works; `peregrinepalaver.space/generate.php` returns a real Cloudflare 403 in SDK and curl. | Use a working connection/solver, then verify the minted token and media chain. |
| NowHDTime | `nhdapi.com` times out in SDK and curl. Frontend is reachable. Its VidNest alternative was researched but sampled backends return 404/502 or a Cloudflare-blocked GoodStream embed. | Recover API access or find a truly playable alternate. No unverified fallback was added. |
| Peckle | No `PECKLE_FEBBOX_COOKIE`. Anonymous metadata works; authenticated quality flow is unverified. | User configures their own FebBox `ui=` cookie locally, then rerun. Never borrow another user's cookie. |
| Stellar | Signed HLS masters resolve; child playlists at `cdn.reallyfast.ch` repeatedly return 502, also with curl. | Retry after service recovery or on a working connection; do not permanently ban. |
| VixSrc | Genuine Cloudflare challenges on site, API and media. A current signed-playlist extractor reference was found. | Configure access/solver, then adapt/verify the signed path; legacy free-tier route is not proven. |

The user was asked asynchronously about an existing proxy/FlareSolverr and
about configuring their own Peckle cookie; no settings were supplied.
`127.0.0.1:8191` was not listening. Docker exists but this account cannot
access its socket. No service/container was installed or launched.
Do not repeatedly ask for the same permission or claim a permanent outage.

## Audit tooling and evidence provenance

`scripts/verify_playback.py` resolves through the actual CLI and decodes using
FFmpeg into null output. It requires exit 0, >=24 frames, >=7.8 seconds for the
default eight-second sample, and nonzero decoded audio. All 41 passing samples
have >=190 frames. Use the preferred cases file to reproduce known samples.
It forwards every playback header, serializes decodes per host, caps process
runtime and stores detailed logs privately under `/tmp`.

After setting TMDB credentials and optional provider credentials in env:

```sh
cargo build -p vsources-cli
python3 scripts/verify_playback.py \
  --cases docs/audits/2026-09-26-playback-cases.json \
  --titles 2 --cards 2 --workers 2 \
  --logs /tmp/vsources-playback-rerun \
  --output /tmp/vsources-playback-rerun.jsonl
```

**Build the CLI separately and wait before auditing.** A command combining
`-p vsources-cli -p vsources --example ...` only built the example, causing
stale CLI audits in early exploratory phases. The selected evidence records
binary hashes so that history is explicit.

Final status is assembled in this order:

1. `2026-09-26-playback-final.jsonl`: all 47, 38 passes.
2. `...-followup.jsonl`: NikaStream and PlayIMDB pass after fixes → 40.
3. `...-mobile.jsonl`: MovieBox passes with runtime key → 41.

Older phase files are exploratory history, not current final status. Codec
summaries are preserved in the consolidated JSON; detailed local logs may be
removed by normal `/tmp` cleanup. Reports exclude signed URLs and cookies.
MovieBox took ~39 seconds to decode its eight-second sample, so real-time
throughput is unproven. NikaStream resolved in ~27 seconds in its followup.

## Validation

**814 tests passed: 808 across all targets, plus 6 doctests.**
Formatting, Clippy with warnings denied and rustdoc with warnings denied pass.
Logs: `/tmp/vsources-final-{tests,doctests,clippy,doc}.log`.

```sh
cargo fmt --all --check
cargo test --workspace --all-targets
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
```

Tests cover the live response/parser changes, independent crypto vectors,
binary wire transport, dynamic relay discovery, no provider media-body fetches,
Unicode labels, mobile session reuse and upgrade-notice filtering. Existing
liveness tests cover bounded reads, lying MIME, archives, redirects, ranges,
cache isolation, cancellation, TTLs and partial/progressive results.

## Retained engine rules and broader gaps

- Shared liveness follows up to three HLS playlist levels plus a segment,
  or reads a 2 KiB direct-file prefix; playlist reads cap at 64 KiB even if
  Range is ignored. Unknown network/IP/CF/429/5xx outcomes remain eligible.
  Six probe permits, four-second budgets, verdict TTLs alive/dead/unknown
  5 min/30 s/5 s bounded by stream TTL, 4096-entry cache, no permanent bans.
  `EngineBuilder::probe_config` configures or disables it. Custom fetchers
  can implement `Fetcher::probe`; default None performs no I/O. Binary API
  adapters also need a raw-byte probe implementation.
- Extractor caches include media/episode and Referer context. VidKing
  coalesces per-media fallback work. Ordinary resolve avoids unused
  progressive snapshots. Keep the engine's Send-safe boxed futures before
  `buffer_unordered`.
- Netlio/NowHDTime retain older local validation behavior worth reviewing.
  Four generic extractor stubs and the 25 upstream classes excluded by the
  older language policy remain broader work; see the earlier
  [quality audit](docs/audits/2026-09-26.md). Do not equate module parity or
  mocked fixtures with live playback or language coverage.
- Secrets via env/flags only: TMDB, FlareSolverr, Peckle and MovieBox config.
  Never persist real signing keys, user cookies or signed media URLs.
- CLI `fetch --head N` limits printed output, not body download size; use
  bounded `probe` for binary/media checks.
- Workspace denies unsafe, unwrap/expect and missing docs. Graph MCP tools
  were unavailable; code discovery used file search. Context7 documentation
  was fetched for library-specific questions. GitHub code search worked.
