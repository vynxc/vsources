//! Current anonymous player protocol; reference: smy778/EncDecEndpoints's
//! `samples/vidfast.py` (updated 2026-08-21), live checked 2026-10-03.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use fancy_regex::Regex;
use serde_json::{Value, json};
use url::Url;
use vsources_core::traits::{FetchRequest, ResolveCtx};
use vsources_core::types::{AudioSelection, CountryCode, Format, MediaRef, Stream, SubtitleTrack};

const ORIGIN: &str = "https://vidfast.vc";
const CODEC: &str = "https://enc-dec.app/api";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\\"(?:en|token)\\":\\"([^"\\]+)\\""#)
        .unwrap_or_else(|e| panic!("valid player token pattern: {e}"))
});

async fn fetch(ctx: &ResolveCtx<'_>, request: FetchRequest) -> Option<String> {
    let response = ctx
        .fetcher
        .request(
            request
                .with_header("User-Agent", UA)
                .with_timeout(Duration::from_secs(8)),
        )
        .await
        .ok()?;
    response.is_success().then_some(response.body)
}
fn result(text: &str) -> Option<Value> {
    let data: Value = serde_json::from_str(text).ok()?;
    if data.get("status").and_then(Value::as_u64) != Some(200) {
        return None;
    }
    data.get("result").cloned()
}
fn endpoint(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https" && url.host_str() == Some("vidfast.vc")).then_some(url)
}
async fn decrypt(ctx: &ResolveCtx<'_>, text: String) -> Option<Value> {
    let request = FetchRequest::post(
        Url::parse(&format!("{CODEC}/dec-vidfast")).ok()?,
        json!({"text":text}).to_string(),
    )
    .with_header("Content-Type", "application/json");
    result(&fetch(ctx, request).await?)
}

pub(super) async fn resolve(ctx: &ResolveCtx<'_>, media: &MediaRef, id: u64) -> Vec<Stream> {
    let path = media.season.map_or_else(
        || format!("/movie/{id}"),
        |season| format!("/tv/{id}/{season}/{}", media.episode.unwrap_or(1)),
    );
    let Some(page) = Url::parse(ORIGIN)
        .ok()
        .and_then(|base| base.join(&path).ok())
    else {
        return Vec::new();
    };
    let Some(html) = fetch(ctx, FetchRequest::get(page)).await else {
        return Vec::new();
    };
    let Some(token) = TOKEN
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|m| m.get(1))
        .map(|m| m.as_str().to_string())
    else {
        return Vec::new();
    };
    let Some(mut codec) = Url::parse(&format!("{CODEC}/enc-vidfast")).ok() else {
        return Vec::new();
    };
    codec.query_pairs_mut().append_pair("text", &token);
    let Some(parts) = fetch(ctx, FetchRequest::get(codec))
        .await
        .and_then(|text| result(&text))
    else {
        return Vec::new();
    };
    let Some(servers) = parts
        .get("servers")
        .and_then(Value::as_str)
        .and_then(endpoint)
    else {
        return Vec::new();
    };
    let Some(stream) = parts
        .get("stream")
        .and_then(Value::as_str)
        .and_then(endpoint)
    else {
        return Vec::new();
    };
    let Some(csrf) = parts
        .get("token")
        .and_then(Value::as_str)
        .filter(|v| !v.contains(['\r', '\n']))
    else {
        return Vec::new();
    };
    let browser = |request: FetchRequest| {
        request
            .with_header("Referer", format!("{ORIGIN}/"))
            .with_header("X-Requested-With", "XMLHttpRequest")
            .with_header("X-CSRF-Token", csrf)
    };
    let Some(encrypted) = fetch(ctx, browser(FetchRequest::post(servers, String::new()))).await
    else {
        return Vec::new();
    };
    let Some(servers) = decrypt(ctx, encrypted)
        .await
        .and_then(|v| v.as_array().cloned())
    else {
        return Vec::new();
    };
    // Try the two first-party main servers first; fallback servers are fetched
    // only if neither yields an English card. Bound each pair concurrently.
    for batch in servers.chunks(2) {
        let results = futures::future::join_all(batch.iter().map(|server| async {
            let data = server.get("data")?.as_str()?;
            let mut url = stream.clone();
            url.path_segments_mut().ok()?.push(data);
            let encrypted = fetch(ctx, browser(FetchRequest::post(url, String::new()))).await?;
            let decoded = decrypt(ctx, encrypted).await?;
            card(
                &decoded,
                id,
                server
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Server"),
            )
        }))
        .await;
        let cards: Vec<_> = results.into_iter().flatten().collect();
        if !cards.is_empty() {
            return cards;
        }
    }
    Vec::new()
}

fn card(data: &Value, id: u64, server: &str) -> Option<Stream> {
    let actual = data
        .get("tmdbId")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))?;
    if actual != id {
        return None;
    }
    let index = u32::try_from(data.get("englishTrackIndex")?.as_u64()?).ok()?;
    let url = Url::parse(data.get("url")?.as_str()?).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let format = if data.get("mp4") == Some(&Value::Bool(true)) {
        Format::Mp4
    } else {
        Format::Hls
    };
    let mut stream = Stream::new(url, format)
        .with_ttl(Duration::from_secs(300))
        .with_label(format!("VidFast · {server} · English"));
    stream.meta.languages = vec![CountryCode::Multi, CountryCode::En];
    stream.meta.audio_selection = Some(AudioSelection {
        language: CountryCode::En,
        audio_index: index,
    });
    stream.meta.source_id = Some("vidfast".into());
    stream.meta.source_label = Some("VidFast".into());
    if data.get("noReferrer") != Some(&Value::Bool(true)) {
        stream.meta.request_headers = BTreeMap::from([("Referer".into(), format!("{ORIGIN}/"))]);
    }
    stream.meta.subtitles = data
        .get("tracks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|track| {
            let url = Url::parse(track.get("file")?.as_str()?).ok()?;
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            let label = track
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(SubtitleTrack {
                url,
                language: label.clone(),
                label,
            })
        })
        .collect();
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_exact_identity_and_english_index_without_forwarding_csrf_to_media() {
        let data = json!({"tmdbId":27205,"url":"https://cdn.example/master.m3u8","englishTrackIndex":1,"noReferrer":true,"tracks":[{"file":"https://subs.example/en.vtt","label":"English"}]});
        let stream = card(&data, 27205, "vRapid").unwrap_or_else(|| panic!("valid card"));
        assert_eq!(stream.meta.audio_selection.map(|s| s.audio_index), Some(1));
        assert!(stream.meta.request_headers.is_empty());
        assert_eq!(stream.meta.subtitles.len(), 1);
        assert!(card(&data, 999, "vRapid").is_none());
        let mut foreign = data.clone();
        foreign["englishTrackIndex"] = json!(-1);
        assert!(card(&foreign, 27205, "vRapid").is_none());
        assert!(endpoint("https://other.example/api").is_none());
    }
    #[test]
    fn reads_escaped_next_payload_and_rejects_api_errors() {
        let html = r#"<script>self.__next_f.push([1,"{\"en\":\"opaque-text\"}"])</script>"#;
        assert!(TOKEN.is_match(html).unwrap_or(false));
        assert!(result(r#"{"status":503,"result":"bad"}"#).is_none());
    }
    struct ProtocolFetcher {
        calls: std::sync::Mutex<Vec<FetchRequest>>,
    }
    #[async_trait::async_trait]
    impl vsources_core::traits::Fetcher for ProtocolFetcher {
        async fn request(
            &self,
            request: FetchRequest,
        ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError>
        {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let body=match request.url.path(){
                "/tv/1396/2/1"=>r#"<script>self.__next_f.push([1,"{\"en\":\"test-seed\"}"])</script>"#.to_string(),
                "/api/enc-vidfast"=>json!({"status":200,"result":{"servers":"https://vidfast.vc/api/servers","stream":"https://vidfast.vc/api/video","token":"anonymous-csrf"}}).to_string(),
                "/api/servers"=>{
                    assert_eq!(request.headers.get("X-CSRF-Token").map(String::as_str),Some("anonymous-csrf"));
                    "server-cipher".into()
                },
                "/api/video/opaque-id"=>"stream-cipher".into(),
                "/api/dec-vidfast"=>{
                    let posted:Value=serde_json::from_str(&request.body.unwrap_or_default()).unwrap_or_else(|e|panic!("fixture JSON: {e}"));
                    let result=if posted["text"]=="server-cipher" {json!([{"name":"vRapid","data":"opaque-id"}])}
                        else {json!({"tmdbId":1396,"url":"https://cdn.example/episode-s2e1.m3u8","englishTrackIndex":1,"noReferrer":true})};
                    json!({"status":200,"result":result}).to_string()
                },
                _=>return Err(vsources_core::error::FetchError::NotFound{url:request.url}),
            };
            Ok(vsources_core::traits::FetchResponse {
                url: request.url,
                status: 200,
                headers: BTreeMap::new(),
                body,
            })
        }
    }
    #[tokio::test]
    async fn native_tv_handshake_keeps_identity_and_audio_selection() {
        let fetcher = ProtocolFetcher {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(vsources_core::types::MediaId::Tmdb(1396), 2, 1);
        let streams = resolve(&ctx, &media, 1396).await;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.path(), "/episode-s2e1.m3u8");
        assert_eq!(
            streams[0]
                .meta
                .audio_selection
                .as_ref()
                .map(|s| s.audio_index),
            Some(1)
        );
        assert!(streams[0].meta.request_headers.is_empty());
        assert_eq!(
            fetcher
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
                .url
                .path(),
            "/tv/1396/2/1"
        );
    }
}
