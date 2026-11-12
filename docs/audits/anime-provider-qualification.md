# Anime provider qualification — 12 November 2026

This corrects the earlier VOD audit’s mistaken anime exclusion. All 17 anime-only SDK routes and StreamXTV’s mixed anime path were included. Licensing was recorded for provenance and notices and was not an eligibility filter. The completed VOD measurements in [issue 9](https://github.com/viptv-org/vsources/issues/9) remain separate.

This is a bounded server-side evaluation. It does not certify deployed beta playback, physical TVs, whole-title reliability, or spoken-language accuracy. No providers were enabled or disabled, and no accounts, preferences, deployments, or production settings were changed.

## Coverage and identities

The pinned SDK320645b0 worker ran 36 source samples: one verified Frieren episode in English-selection and ordinary all-audio modes across all 18 routes. There were **23 resolved, 6 empty, and 7 timeout/error results**. “Resolved” establishes discovery only. The mode name “sub” means the all-audio request; it is not proof of subtitled playback.

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

All selected audio streams were untagged in the decoder evidence. Five dub cases had provider-declared English DUB contexts and decoded audio/video, but English speech was not independently established. Five ordinary cases had provider-declared Japanese audio with English captions; the decoded audio language remained unknown. Subtitles never establish dubbed audio.

For AnimeKai’s three ordinary cases, a bounded 4096-byte caption prefix contained parseable WebVTT timed cues on an English-declared track. AnikotoTV’s two ordinary caption samples were not verified. No subtitle rendering, translation accuracy, or spoken-language listening assessment was performed.

## Confirmed repairs and research limits

- [Issue17 / PR21](https://github.com/viptv-org/vsources/pull/21) repairs the confirmed ReAnime current-API/cover-identity mismatch, preserves canonical non-base-season identity, and stops the whole resolve on authentication, access, or rate-limit denials. Ordinary obsolete, unavailable, transient, or HTML routes retain bounded fallback. A current primary GitHub implementation and capped metadata diagnosis support this contract change.
- [Issue22 / PR24](https://github.com/viptv-org/vsources/pull/24) repairs a concrete bounded Matroska false negative: a complete enabled English audio entry preceded a later incomplete subtitle entry inside the existing 8KiB prefix. It preserves audio ordinals, complete-entry and parent bounds, disabled tracks, and authoritative LanguageIETF handling. This establishes bounded track metadata, not whole-file validity or spoken-language certification.
- [Issue23](https://github.com/viptv-org/vsources/issues/23) removes invented ReAnime language, codec, and quality defaults. Verified embedded-English selection remains explicit.

Empty results and timeouts on other routes do not establish stale contracts or global outages. AniWaves’ bounded metadata comparison encountered a redirect without proving a replacement endpoint. Read-only GitHub code search also examined AniChan, AniWaves, AnimeSuge, NikaStream, Itachi and StreamXTV leads; the later scoped searches reached GitHub’s API search rate limit. Search leads were not treated as validated implementations, and no credentials or mirrors were rotated to bypass limits. Existing primary references were sufficient for the confirmed ReAnime repair. AllAnime remains an unmeasured research reference without an integrated/qualified adapter; it was not rejected on licensing grounds.

Final merged-source ReAnime validation is pending and will be recorded before closing this audit. Earlier privately built workers whose exact checkout provenance could not be established after shared-target artifact reuse are excluded from repaired-revision qualification.

## Budgets and limits

The baseline discovery retained 7,698,626 completed response-body bytes. Its 32MiB between-attempt stop is an observed-body soft stop, not a hard wire cap; incomplete responses and transport overhead are not included. Native worker request, redirect, response, provider-timeout, and 8KiB probe limits remained in force.

The shortlist consumed **31,440,824 measured relay bytes** under a 128MiB campaign ceiling. Each selected card shared a 10MiB limit across strict and override attempts, with 8 seconds of media and a 25-second decoder timeout. Small caption samples had separate bounded relay allowances inside the same aggregate. Relay counters count consumed socket/TLS bytes, excluding TCP/IP/DNS and possible kernel-prefetch overhead; they are not packet-wire accounting. One worker ran under the shared TV validation lock.

Standalone resolution and software decode can have different egress/IP-binding behavior from the deployed gateway. No server result is promoted to deployed or physical-device qualification. Raw locations, signed queries, request values, private logs and fixtures are excluded. The sanitized per-sample measurements are in [anime-provider-measurements.json](anime-provider-measurements.json).
