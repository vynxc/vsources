# PlayIMDB and MovieBox web contract checks

## PlayIMDB

The provider resolves IMDb references to numeric TMDB IDs before requesting the
stream API. The request uses that resolved value under `tmdb`; it never puts an
IMDb `tt…` value in the TMDB namespace. Header and result-publication policies
are unchanged. Quality/audio remain unknown without structured evidence.

Current primary sources:

- [VaPlayer implementation](https://github.com/Inside4ndroid/TMDB-Embed-API/blob/main/providers/vaplayer.js), blob `6fcdba588f684f7a81674292983f46f5df627e06`: IMDb API lane and exact player-page Referer.
- [NiakVIO recipe restoration](https://github.com/niakw/NiakVIO/blob/main/scripts/restore_playimdb_proof_v5.py), blob `a6086b35780df142e64a9145e13208c9de8f80e8`: historical TMDB lane, nextgen request context. Its retained-proof claims are not independent playback evidence.
- [Tmovie request construction](https://github.com/ziedsG1/Tmovie/blob/main/src/lib/stream.ts), blob `cfd40a13441f230bf2cba87faeb9acf38649bc33`: IMDb/brightpath request context. Broad Referer sweeps are not adopted.

Bounded fresh Inception checks on the standalone server path:

| Request | API result | First media result |
| --- | --- | --- |
| Numeric TMDB, nextgen context | HTTP200 / API200, three cards | HTTP401 |
| IMDb namespace, nextgen player context | HTTP200 / API200, three cards | HTTP401 |
| IMDb namespace, brightpath player context | HTTP200 / API200, three cards | Not requested |
| Both ID parameters | HTTP200 / API200, zero cards | Not requested |
| IMDb value incorrectly sent under `tmdb` | HTTP200 / API404, zero cards | Not requested |

The identity propagation fix repairs the last request construction. It does not
establish a repair for CDN401. Both correctly namespaced samples identified the
requested title/IMDb in API metadata, but returned no playable bytes on this
path. No credentials, token fabrication, access workaround or deployed-egress
claim is included.

## MovieBox legacy/web

Current default-branch source leads were inspected:

- [WBM source](https://github.com/h9wrd/wbm/blob/main/src/source/MovieBox.ts), blob `0f35860d14ddf3fbb517ea3d3738fb6378de96c6`, and [extractor](https://github.com/h9wrd/wbm/blob/main/src/extractor/MovieBox.ts), blob `002a3d6a170259da56d4a9b90ba7c04b3fc3d504`: public download endpoint and videodownloader Referer. Its first-resource identity fallback is not adopted.
- [Web wrapper](https://github.com/faisaljs/Moviebox-API/blob/main/upstream.py), blob `7feda82bb09c1caf991e4d9b8476f22f4a11acfb`: service-issued guest token and advertised player-domain discovery. Repository declares MIT.
- [Mobile resource-position lead](https://github.com/mesamirh/MovieBox-Tui/blob/main/src/providers/moviebox/mod.rs), blob `9f9d625fd2352125b9d4243ec3fbf1ca93a239ca`: a separate mobile API path. No mobile signing key or configuration is imported.

The normal guest flow returned one exact title/year/type match. Anonymous
search returned400; service-issued guest-token search returned200. Public
download returned200 with three file declarations; legacy play and the
advertised native player context each returned200 with one declaration.
First-media range reads for all three routes returned426 with `426 Forbidden`
and no Upgrade header. Removing Referer, as another primary implementation
suggests for direct MP4, returned429; further media attempts stopped.

No tested URL/Referer change demonstrated a MovieBox playback repair, so its
implementation and configuration remain unchanged. These outcomes describe the
bounded standalone server vantage, not all namespaces, deployed proxy/WARP
routes or provider-wide availability. No media decoded, audio-language
qualification, account/cookie modification, authentication/DRM bypass or mobile
key inference is claimed. Raw URLs, guest tokens, headers and payloads remain
private. Source notices/provenance are retained; licensing did not filter the
research leads.
