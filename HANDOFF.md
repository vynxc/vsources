# Handoff — provider playback verification (2026-09-26)

Workspace: `/mnt/ALPH/code/vsources`; `/home/vynxc/code/vsources` resolves here.
**There are no commits and the original project is entirely untracked.**
Preserve the tree. Do not reset or remove untracked files as cleanup.

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
