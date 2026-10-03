# Provider repair investigation — 2026-10-03

## Quick follow-up: MovieBox key

The public app-wide signing constant was found in
[MovieBox-Tui crypto.rs](https://github.com/mesamirh/MovieBox-Tui/blob/751fd0ec49e114d0ae849bf1dc52fec869d58e7d/src/providers/moviebox/crypto.rs)
and configured in ignored `.env.local`; it is not an account credential.
The mobile app version was aligned to the current reference's 50020121.
Two one-movie checks resolved mobile media in 0.85–0.90 seconds and started
eng-tagged playback around 2.3 seconds, but stalled before completing eight
seconds within a 25-second budget. **MovieBox is not yet a playback pass.**
[Sanitized before/after evidence](2026-10-03-moviebox-key-check.json).

The quick GitHub follow-up found no other ready-to-copy repair. The main
upstream remains at the previously reviewed commit. EasyProxy's FlareSolverr
integration requires runtime provisioning; a Cineby lead currently points to
CineJoy in the referenced project's tree, which is already represented here.
No account cookies or private credentials were copied, and deeper work was
skipped under the user's one-minute/easy-options constraint. Older matrix and
investigation notes below describe their original configuration, before this key.


Reviewed every one of the 22 providers with zero qualifying playback in the
October 2 matrix. The current SDK contains 46 providers: AniBD and AnimeZeY
were removed. Fast anime with English subtitles remains in scope; sub-only
status itself is not grounds for removal.

Four repairs are verified in full 20-title SDK reruns:
- VixSrc: current signed player API and required English audio selection,
  **10/10 movie/TV samples**; the ten anime samples were misses.
- FrameXTV: unwrap its API media proxies, retain Origin/Referer, and inspect
  bounded HLS/TS metadata to select English or drop known foreign-only audio.
  **10/10 movie/TV samples**; eight selected eng tags, two und/untagged selections
  remain metadata-level uncertainty. The ten anime samples were misses.
- PrimeShows: current domain, new TV query route and its explicit VidLink
  server through the working native adapter, **9/10 movie/TV samples**.
- WatchSeries: its explicit VidLink server through that same adapter,
  **9/10 movie/TV samples**. These adapters share an upstream; they are not
  independent catalog/storage sources. Both missed the sampled Twilight Zone episode.

VidFast's new native protocol is implemented and tested with fixtures. A separate
curl protocol probe decoded eight seconds with selected eng audio, but the SDK
client receives a genuine Cloudflare challenge on vc/pro. The reachable bz alias
returned a server error with the referenced codec. Its complete SDK rerun remained
20/20 empty, so it is **not** counted as repaired or added to the environment.
No clearance, session cookie or signing key was borrowed or bundled.

Removed:
- **AniBD**, under the user's updated instruction permitting removal if the sub
  route failed: its site reports it has stopped and the episode API returned 404
  for Cowboy Bebop, Death Note, Attack on Titan, Frieren and Solo Leveling IDs.
- **AnimeZeY**, under the language policy: live results and the upstream audio
  labeling show a Portuguese-focused anime catalog. It was not removed simply
  for Japanese audio; suitable English subtitles would be eligible.

The other retained providers remain unresolved, not declared permanently dead.
MovieBox requires the caller's mobile signing key for its mobile path; desktop
playback mostly returned 426. Peckle needs an owned FebBox cookie. Shared
speedracelight seed requests returned 502; NowHDTime timed out; AniChan's fresh
sessions produced empty server lists; the other fresh probes did not establish
playback or a verified header fix. Recent upstream changes already implemented
in the SDK are distinguished from actual new repairs in the table below.

The audit now rejects positively tagged non-English movie/TV audio as well as
non-English selections in anime dub mode. This also corrects an older RiveStream
The Matrix transport pass whose selected audio was Hindi. Out-of-scope catalog
matches retain wrong_catalog priority. Untagged audio and speech/title identity
are not independently verified; inspect each case's evidence before interpreting
a transport pass as stronger proof.

Current matrix: **46 × 20 = 920 rows**, **221 playback passes**, and **480 earlier rows retained**. `.env.generated` is regenerated from qualifying current rows. Historical retired-provider rows remain in history; the original snapshot is preserved.

[Interactive report](2026-10-02-matrix/index.html) · [Research JSON](2026-10-03-provider-repairs.json) · [Targeted diagnostics](2026-10-02-provider-diagnostics.json) · [Netlio full recheck](2026-10-02-netlio-recheck/report.json)

## All 22 outcomes

| Provider | Outcome | Findings | References |
|---|---|---|---|
| AniBD | removed | Site reports it has stopped; episode API returned 404 for five known anime IDs. Removed after the user approved removing it if its sub route did not work. | [AniBD.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/AniBD.js) · [anibd.py](https://github.com/aryaniiil/anime-api/blob/d9a785ad030d6fc6a8ad709291be9612c864449e/src/providers/anibd.py) |
| MovieBox | configuration required | Desktop cards fail playback (mostly HTTP 426). The existing mobile route requires the caller’s MOVIEBOX_MOBILE_SIGNING_KEY; only TMDB configuration is present. No key was bundled or borrowed. | [MovieBox.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/MovieBox.js) · [index.ts](https://github.com/streamn-noah/moviebox-api/blob/a50b7437babd5f986b095ec08f529a0f94631a0b/src/index.ts) |
| Necro | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [Necro.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Necro.js) |
| Netlio | unresolved | Current GitHub catalog is maintained; Inception is absent and sampled series links fail the liveness path. All twenty fresh SDK recheck cases remain empty. No newer working protocol change was found. | [Netlio.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Netlio.js) |
| NowHDTime | unresolved | Current extension source still references nhdapi/vidnest. The nhdapi service times out here; targeted SDK tests time out. No confirmed playable alternate was found. | [Nowhdtime.kt](https://github.com/salmanbappi/sb-extensions-source/blob/463d5325d2ef969b7292716261ae3e6346c75aae/src/all/nowhdtime/src/eu/kanade/tachiyomi/animeextension/all/nowhdtime/Nowhdtime.kt) |
| 2Peckle | configuration required | The upstream FebBox quality flow requires the caller’s PECKLE_FEBBOX_COOKIE. No owned session is configured. Anonymous metadata does not establish playback. | [Peckle.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Peckle.js) |
| PrimeShows | repaired | Migrated to www.primeshows.org and the new TV query format. Current app explicitly offers VidLink; resolves that server through the working native adapter. Nine movie/TV samples pass. | [PrimeShows.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/PrimeShows.js) |
| VidFast | native added blocked | New native player/CSRF/codec protocol is implemented and fixture tested. Separate curl protocol probe decoded eight seconds with eng audio; the SDK client receives a genuine Cloudflare challenge on vc/pro. The reachable bz alias returned a server error with the referenced codec. Twenty SDK rerun rows remain empty; not counted as repaired. | [VidFast.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VidFast.js) · [vidfast.py](https://github.com/smy778/EncDecEndpoints/blob/f78498e8a3ffb5a326ba9a0ab82b595880365a28/samples/vidfast.py) |
| VidKing | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [VidKing.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VidKing.js) |
| VidSrcSbs | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [VidSrcSbs.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VidSrcSbs.js) |
| WatchSeries | repaired | Its explicit VidLink server now resolves through the native adapter before the failing generic backend, preserving exact TV identity. Nine movie/TV samples pass. Shares the VidLink upstream with PrimeShows and the standalone route. | [WatchSeries.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/WatchSeries.js) |
| AniChan | unresolved | Anonymous session bootstrap succeeds; episode metadata advertises sub/dub, but current server requests return no cards. Targeted Cowboy Bebop/Frieren SDK checks remain empty. The September domain/session updates were already ported. | [AniChan.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/AniChan.js) |
| AnimeZeY | removed | Portuguese-focused catalog: live Naruto results include Portuguese titles/dubs, consistent with the upstream Portuguese/Japanese audio flags. Removed under the language policy. | [animezey.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/animezey.cjs) · [AnimeZeY.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/AnimeZeY.js) |
| AcerMovies | unresolved | Current api2 search/quality protocol was already ported. Targeted fresh SDK movie calls remain empty; upstream documents unresolved source cache and per-IP quota issues. English/dual-audio catalog evidence does not justify language-based removal. | [AcerMovies.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/AcerMovies.js) |
| Atlantic | unresolved | September gate/CDN protocol changes were already ported. All three targeted fresh resolves are empty; no new independently playable endpoint was confirmed. | [atlantic.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/atlantic.cjs) · [Atlantic.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Atlantic.js) |
| Cineby | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [cineby.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/cineby.cjs) · [Cineby.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Cineby.js) |
| FrameXTV | repaired | Unwraps the owned API media proxy and carries Origin/Referer; retains explicit Origin headers. Bounded HLS/TS metadata selects English audio and rejects known foreign-only tracks. Anime with Japanese audio and English subtitle metadata remains eligible outside dub-only mode. | [framextv.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/framextv.cjs) · [FrameX.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/FrameX.js) |
| Stellar | unresolved | Challenge/resolve flow returns signed cards, but three targeted tests fail playback with current headers. Upstream/CDN failure remains; no successful header repair was found. | [stellar.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/stellar.cjs) · [Stellar.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/Stellar.js) |
| VidEasy | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [videasy.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/videasy.cjs) · [VidEasy.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VidEasy.js) |
| VideasyTo | unresolved | Current references still depend on the shared speedracelight seed/backend or generic embed fallback. Fresh seed requests return 502; targeted SDK resolves are empty or time out. Newer codec samples did not establish an accessible replacement backend. | [videasyto.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/videasyto.cjs) · [VideasyTo.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VideasyTo.js) |
| VixSrc | repaired | Replaced fabricated empty-token playlist with the current API → signed embed → HLS flow; verifies and selects English audio. Ten movie/TV samples pass. | [VixSrc.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/VixSrc.js) · [vixsrc.py](https://github.com/J0hnBloodborne/Nautilus/blob/5f990b112d2acb32f3d64862358aee614b16eead/src/providers/sources/vixsrc.py) |
| ZXCStream | unresolved | September dynamic fields/token/CryptoJS protocol was already ported. Three fresh SDK lookups are empty; no newer working protocol change was found. | [zxcstream.cjs](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/nuvio/zxcstream.cjs) · [ZXCStream.js](https://github.com/SaugatXthaa/PhoeniX/blob/f882b5a5246620c2b5218e1aa0dca6157815ec00/src/source/ZXCStream.js) |

## Reproduce

Use the existing matrix runner; keep alternate audio policies in a new output
folder. Each worker is frozen by SHA-256, and all raw URLs/headers stay private.

```sh
python3 scripts/provider_matrix.py --resume --rerun vixsrc,primeshows,watchseries,framextv,vidfast --output docs/audits/2026-10-02-matrix
python3 scripts/provider_matrix.py --providers anichan --no-english-dub --output docs/audits/anichan-sub-recheck
```

The targeted diagnostic JSON records its shorter 20-second source budget,
30-second decode budget and one-card sample limit; it is not a replacement for
the full 20-title matrix. The full Netlio recheck also retained zero passes.
Temporary protocol probes and bounded segment prefixes are private under
`/tmp/vsources-zero-research-20261002`; signed URLs, tokens and header values
are not published.
