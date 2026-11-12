# Anime provider qualification — 12 November 2026

This corrects the earlier VOD audit’s mistaken anime exclusion. All 17 anime-only SDK routes and StreamXTV’s mixed anime path were included. Licensing was recorded for provenance and notices and was not an eligibility filter. The completed VOD measurements in [issue 9](https://github.com/viptv-org/vsources/issues/9) remain separate.

This is a bounded server-side evaluation. It does not certify deployed beta playback, physical TVs, whole-title reliability, or spoken-language accuracy. No providers were enabled or disabled, and no accounts, preferences, deployments, or production settings were changed.

## Coverage and identities

The pinned SDK `320645b0` worker ran 36 source samples: one verified Frieren episode in English-selection and ordinary all-audio modes across all 18 routes. There were **23 resolved, 6 empty, and 7 timeout/error results**. “Resolved” establishes discovery only. The mode name “sub” means the all-audio request; it is not proof of subtitled playback.

| Case | Canonical identity | Source identity | Episode handling |
|---|---|---|---|
| Frieren: Beyond Journey’s End (TV, 2023) | TMDB209867 | AniList154587 / MAL52991 | S1E2, absolute2 |
| Cowboy Bebop (TV, 1998) | TMDB30991 | AniList1 / MAL1 | S1E2, absolute2 |
| Akira (film, 1988) | TMDB149 | AniList47 / MAL47 | Movie request, no TV season; provider movie episode1 |

The worker receives numeric TMDB MediaId values. AniList and MAL IDs were verified through canonical mappings and primary metadata, not inserted into the TMDB field. No later-season numbering was guessed. Movie/series deserialization and language classification were checked offline first.

## All-route discovery

Completion timings below are cold source-discovery times, not playback startup times. These single-fixture outcomes do not establish global provider availability.

| SDK route | Family / dependency relationship | English-selection request | All-audio request |
|---|---|---|---|
| aniwaves | EchoVideo AJAX | empty, 9.883s | empty, 7.082s |
| allwish | MegaPlay portal | resolved, 7.417s | resolved, 7.058s |
| anichan | AniList-keyed native API | empty, 6.749s | empty, 7.337s |
| anidoor | MegaPlay templates | resolved, 5.247s | resolved, 4.714s |
| anikage | Composite API/relay with shared backends | resolved, 9.499s | resolved, 7.896s |
| anikoto | MegaPlay portal | resolved, 13.640s | resolved, 8.124s |
| anikototv | MAL-keyed MegaPlay | resolved, 2.872s | resolved, 3.263s |
| animeflix | WordPress + extractor mirrors | resolved, 8.151s | resolved, 8.122s |
| animegg | AnimeGG direct/embed files | resolved, 15.958s | resolved, 5.152s |
| animekai | Otaku/MAL stream family | resolved, 5.016s | resolved, 4.859s |
| animesuge | MegaPlay portal | empty, 3.827s | empty, 4.255s |
| animotvslash | WordPress native/mirror mixture | resolved, 10.542s | resolved, 5.555s |
| hianime | Otaku/AJAX stream family | resolved, 11.328s | resolved, 5.534s |
| itachi | VidHawk primary/MegaPlay secondary | error, 18.002s | resolved, 6.645s |
| 2dhive | MAL-keyed MegaPlay | resolved, 4.621s | resolved, 3.098s |
| nikastream | Anivexa multi-provider aggregator | error, 18.002s | error, 18.002s |
| reanime | FlixCloud/progressive multi-audio | error, 18.002s | error, 18.002s |
| streamxtv | Mixed FrameX + anime MegaPlay/VidNest | error, 18.002s | error, 18.002s |

Portal names are not independent CDN coverage. AllWish, AniDoor, Anikoto, AnikotoTV, AnimeSuge and 2DHive share MegaPlay-family paths; other wrappers can also converge on shared extractors. Route coverage is therefore reported separately from independent supply.

## Shortlist follow-up

AnimeKai and AnikotoTV were selected for repeatable declared-dub and Japanese/English-caption contexts. Across three verified fixtures and both modes, 12 selected cases produced 24 source samples: one fresh engine and one reused-engine sample per case. Ten cases resolved; both AnikotoTV Akira modes were empty immediately with zero requests. The reused-engine timings are cache behavior, not renewed origin availability.

| Route | Fresh resolve range for positive cases | Reused-engine range | Decoded cases |
|---|---:|---:|---:|
| AnimeKai | 3.994–4.747s | 33.53–69.71ms | 6/6 |
| AnikotoTV | 3.237–3.929s | 31.72–65.12ms | 4/6; Akira unsupported/empty in this request path |

Each of the ten positive cases decoded about 8 seconds of audio/video (190–198 video frames). There were 20 decoder attempts: ten strict-policy refusals followed by ten explicitly isolated finite-extension experiments. The installed FFmpeg5.1.9 strict profile rejected MPEG-TS segments named `.jpg`; the separate experiment added only the observed/tested `jpg` suffix to the finite installed allowlist and disabled its extension-picky check. A synthetic MPEG-TS fixture verified that narrow distinction and retained protocol and byte restrictions. **Every successful decode used that policy override and is not shipping-pipeline qualification.** No production guard was changed.

In those policy-override attempts, first decoded-video progress arrived 1.916–2.697s after decoder launch for AnimeKai (median2.410s, n=6) and 1.440–1.737s for AnikotoTV (median1.624s, n=4). These timings exclude source resolution and the preceding strict refusal; they are not deployed end-to-end startup times.

All selected audio streams were untagged in the decoder evidence. Five dub cases had provider-declared English DUB contexts and decoded audio/video, but English speech was not independently established. Five ordinary cases had provider-declared Japanese audio with English captions; the decoded audio language remained unknown. Subtitles never establish dubbed audio.

For AnimeKai’s three ordinary cases, a bounded 4096-byte caption prefix contained parseable WebVTT timed cues on an English-declared track. AnikotoTV’s two ordinary caption samples were not verified. No subtitle rendering, translation accuracy, or spoken-language listening assessment was performed.

## ReAnime after the confirmed repairs

The combined worker was forced to compile the current core, provider and worker sources at **SDK `b9b84e9f949839010257f52ba88e226f1bf2c9d0`**. Exact artifact manifests and `fresh:false` were checked, and the native probe remained 8KiB. One verified Frieren episode was resolved in both modes, fresh and with the same engine reused: **four source samples, all resolved**.

| Request | Fresh resolve | Reused engine | Required audio selection | Decoder-selected tag |
|---|---:|---:|---|---|
| English dub | 4.921s / 5 requests | 21.890ms / 0 requests | English, audio ordinal 1 | `eng` |
| Ordinary all-audio | 2.883s / 4 requests | 22.842ms / 0 requests | None; default audio ordinal 0 | `jpn` |

Both strict progressive-file decodes reached the 16MiB card limit after 152 video frames and 6.465 seconds of decoded audio/video. **Both are inconclusive for the full 8-second target**, with no retry, larger cap, or extension-policy override. First decoded-video progress arrived 2.906s/2.284s after decoder launch respectively, excluding resolution. Each consumed 16,777,216 relay bytes.

The decoder observed H.264 1920×1080 video, AAC audio ordinal 0 tagged Japanese and ordinal 1 tagged English, plus three English-tagged ASS/SRT subtitle streams. The required English ordinal 1 was honored in dub mode; ordinary mode selected Japanese. Subtitle rendering and spoken-language accuracy were not certified. The actual file was Matroska although the SDK uses its progressive `Mp4` format enum; a 720p rendition was not measured.

A harness case-normalization mismatch initially skipped the planned dub decode because SDK `En` was compared with lowercase `en`. It was corrected using the same still-fresh retained context before any dub media request; no source resolve was repeated. These measurements establish the bounded English-track selection repair and partial strict decode, not full-title or deployed playback certification.

The most defensible anime shortlist is therefore conditional: **ReAnime** has the strongest observed audio-track evidence in this single episode but did not complete the 8-second target under its byte cap; **AnimeKai** has repeatable TV/film discovery and English-declared caption-cue evidence, while **AnikotoTV** has faster sampled TV discovery but no Akira result. The latter two only decoded with the isolated policy override, and their spoken audio language remains unverified. Other resolved routes remain discovery-only leads. No automatic enablement or global disable is justified.

## Confirmed repairs and research limits

- [Issue17 / PR21](https://github.com/viptv-org/vsources/pull/21) repairs the confirmed ReAnime current-API/cover-identity mismatch, preserves canonical non-base-season identity, and stops the whole resolve on authentication, access, or rate-limit denials. Ordinary obsolete, unavailable, transient, or HTML routes retain bounded fallback. A current primary GitHub implementation and capped metadata diagnosis support this contract change.
- [Issue22 / PR24](https://github.com/viptv-org/vsources/pull/24) repairs a concrete bounded Matroska false negative: a complete enabled English audio entry preceded a later incomplete subtitle entry inside the existing 8KiB prefix. It preserves audio ordinals, complete-entry and parent bounds, disabled tracks, and authoritative LanguageIETF handling. This establishes bounded track metadata, not whole-file validity or spoken-language certification.
- [Issue23](https://github.com/viptv-org/vsources/issues/23) removes invented ReAnime language, codec, and quality defaults. Verified embedded-English selection remains explicit.

Empty results and timeouts on other routes do not establish stale contracts or global outages. Existing baseline transport telemetry showed only HTTP200 completions for both empty AniChan cases (9 each) and AniWaves cases (11/14). AnimeSuge had six HTTP200 and four HTTP404 completions per empty case. NikaStream timed out with three of five calls completed (one HTTP200 and two transport errors); Itachi’s dub request had two of three calls complete (one HTTP200 and one transport error), while its ordinary request resolved. StreamXTV timed out after 20–21 HTTP404 and 10–14 HTTP200 completions, with one unfinished call per case. The preserved baseline observer did not retain paths or body prefixes, so endpoint-stage and replacement-contract claims cannot be made from these counts. AniWaves’ bounded metadata comparison encountered a redirect without proving a replacement endpoint. Read-only GitHub code search examined AniChan, AniWaves, AnimeSuge, NikaStream, Itachi and StreamXTV. After a temporary search limit reset, targeted filename and contract searches were completed without rotating credentials. The original ported PhoeniX/ignatiusphoenix repositories now return 404; no maintained Itachi replacement contract was established. Current StreamX backend routes and search-result fields still match the Rust adapter. Existing primary references were sufficient for the confirmed ReAnime repair.

The current [AniChan client](https://github.com/MXFia19/module-sora/blob/main/anichan/anichan.js) (blob `b0d49d7662be53b07931db6444e023ab36bc517d`) uses changed episode/category/tier parameters and a session-nonce envelope. A separate three-call ordinary metadata check returned catalog200, episode200, then server401; it stopped there with zero session, credential, decryption or media calls. That is a concrete access boundary, not evidence that copying client constants would qualify playback. No third-party account token or embedded API key was imported.

The inspected AnimeSuge search lead was a decompiled VRF/AJAX client (blob `aa77e31b0b171e914e3a7766714c1ed837e7f744`) for a different frontend protocol, without a confirmed replacement relationship to the configured portal. It was not imported as a fix.

The current [StreamX route](https://github.com/deepanshuhyb/streamX-backend/blob/main/src/routes/data.route.ts) (blob `2f4817f732bb574da1e299ba02fb31e4e70afc2a`) still mounts anime search beneath `/api`; its controller returns the expected results. StreamX’s offline control flow also waits for its 20-provider direct sweep before starting anime embeds. Its 26-second internal sweep horizon can exceed this audit’s 18-second provider budget; the actual current Core/backend and prepared Core consumer also set 18 seconds at `vsources_runtime.rs:287`. This confirmed a scheduling defect, tracked in [issue 29](https://github.com/viptv-org/vsources/issues/29) as a separate owning repair, rather than a reason to relax the timeout.

The current [Anivexa routing implementation](https://github.com/walterwhite-69/Anivexa-API/blob/main/index.js) (blob `ae1e0969dd3c59ede93e967a13365c2fb4533ab6`) retains the episode/watch route shape but has a different provider roster. One capped ordinary request to the existing configured API root returned HTTP200 (1254 body bytes;6992 relay bytes), advertising 15 backends. All 11 SDK backend IDs remain present; four additional backend IDs were unmeasured. No watch/media/session request followed. The deployed roster differs from the GitHub fork, so that fork was not copied as a repair. These comparisons do not justify speculative adapters or bypasses. AllAnime remains an unmeasured research reference without an integrated/qualified adapter; it was not rejected on licensing grounds.

Separate diagnostic work is not folded into the discovery/playback success counts: two public canonical-mapping calls, one AniWaves metadata redirect check, two ReAnime API metadata checks, three AniChan metadata checks, and one NikaStream root/catalog check. One separately capped larger Matroska-prefix diagnostic failed with an HTTP error and did not establish language availability. Two intermediate forced-source ReAnime samples reproduced ordinary resolution versus a bounded English-selection false negative; two earlier source attempts with uncertain exact artifact provenance were retained as attempts and excluded from repaired-revision qualification.

The ReAnime repairs are measured on merged SDK `b9b84e9`. StreamX [PR 30](https://github.com/viptv-org/vsources/pull/30) is a separate scheduling repair; later final-main attribution requires unchanged ReAnime/core/probe/ABI component hashes rather than repeated media for unrelated source history. Earlier privately built workers whose exact checkout provenance could not be established after shared-target artifact reuse are excluded from repaired-revision qualification.

## Budgets and limits

The baseline discovery retained 7,698,626 completed response-body bytes. Its 32MiB between-attempt stop is an observed-body soft stop, not a hard wire cap; incomplete responses and transport overhead are not included. Native worker request, redirect, response, provider-timeout, and 8KiB probe limits remained in force.

The preliminary shortlist consumed 31,440,824 measured relay bytes. After the two ReAnime cards, the same media campaign consumed **64,995,256 measured relay bytes** under its 128MiB ceiling. Each preliminary selected card shared a 10MiB limit across strict and override attempts, with 8 seconds of media and a 25-second decoder timeout. Small caption samples had separate bounded relay allowances inside the same aggregate. Relay counters count consumed socket/TLS bytes, excluding TCP/IP/DNS and possible kernel-prefetch overhead; they are not packet-wire accounting. One worker ran under the shared TV validation lock.

Standalone resolution and software decode can have different egress/IP-binding behavior from the deployed gateway. No server result is promoted to deployed or physical-device qualification. Raw locations, signed queries, request values, private logs and fixtures are excluded. The sanitized per-sample measurements are in [anime-provider-measurements.json](anime-provider-measurements.json).
