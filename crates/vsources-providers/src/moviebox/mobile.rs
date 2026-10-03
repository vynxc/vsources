//! Optional mobile API adapter. The caller supplies the signing key; it is not
//! bundled. Protocol reference: mesamirh/MovieBox-Tui's moviebox client/adapt.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use moka::future::Cache;
use serde_json::{Value, json};
use url::Url;
use vsources_core::traits::{FetchRequest, ResolveCtx};
use vsources_core::types::{CountryCode, Format, MediaRef, Stream};

const HOSTS: &[&str] = &[
    "https://api6.aoneroom.com",
    "https://api.inmoviebox.com",
    "https://api4sg.aoneroom.com",
];
const PREFIX: &str = "/wefeed-mobile-bff";
const UA: &str = "com.community.oneroom/50020121 (Linux; U; Android 13; en_US; 23078RKD5C; Build/TQ2A.230405.003; Cronet/135.0.7012.3)";
const REFERER: &str = "https://sportslive.wine";

#[derive(Clone)]
struct Session {
    base: &'static str,
    token: String,
}

pub(super) struct Client {
    key: Vec<u8>,
    info: String,
    session: Cache<(), Session>,
}

impl Client {
    pub(super) fn from_env() -> Option<Self> {
        let key = std::env::var("MOVIEBOX_MOBILE_SIGNING_KEY").ok()?;
        let key = key.trim();
        if key.len() < 32 || !key.len().is_multiple_of(2) {
            return None;
        }
        let bytes = key
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Self::new(bytes))
    }

    pub(super) fn new(key: Vec<u8>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let device = hex(&Md5::digest(
            format!("vsources:{now}:{}", std::process::id()).as_bytes(),
        ));
        let gaid = format!(
            "{}-{}-{}-{}-{}",
            &device[..8],
            &device[8..12],
            &device[12..16],
            &device[16..20],
            &device[20..]
        );
        Self {
            key,
            info: json!({"package_name":"com.community.oneroom","version_name":"4.0.01.0813.03","version_code":50_020_121,"os":"android","os_version":"13","install_ch":"ps","device_id":device,"install_store":"ps","gaid":gaid,"brand":"Xiaomi","model":"23078RKD5C","system_language":"en","net":"WIFI","region":"US","timezone":"America/New_York","sp_code":"40401","X-Play-Mode":"2"}).to_string(),
            session: Cache::builder().max_capacity(1).time_to_live(Duration::from_hours(1)).build(),
        }
    }

    async fn request(
        &self,
        ctx: &ResolveCtx<'_>,
        base: &str,
        path: &str,
        body: Option<String>,
        token: Option<&str>,
    ) -> Option<Value> {
        let url = Url::parse(&format!("{base}{PREFIX}{path}")).ok()?;
        let method = if body.is_some() { "POST" } else { "GET" };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis()
            .to_string();
        let signature = signature(&self.key, method, &url, body.as_deref(), &timestamp)?;
        let reversed: String = timestamp.chars().rev().collect();
        let client_token = format!("{timestamp},{}", hex(&Md5::digest(reversed.as_bytes())));
        let mut request = FetchRequest::get(url)
            .with_timeout(Duration::from_secs(8))
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_header("Content-Type", "application/json")
            .with_header("X-Client-Token", client_token)
            .with_header("X-Tr-Signature", signature)
            .with_header("X-Client-Info", &self.info)
            .with_header("X-Client-Status", "0");
        request.method = method.into();
        request.body = body;
        if let Some(token) = token {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let value: Value = response.json().ok()?;
        Some(value.get("data").cloned().unwrap_or(value))
    }

    pub(super) async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        name: &str,
        year: Option<u16>,
    ) -> Option<Vec<Stream>> {
        let session = self
            .session
            .try_get_with((), async {
                for base in HOSTS {
                    if let Some(response) = self
                        .request(
                            ctx,
                            base,
                            "/user-api/visitor-login",
                            Some("{}".into()),
                            None,
                        )
                        .await
                        && let Some(token) = response
                            .get("token")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    {
                        return Ok(Session {
                            base,
                            token: token.into(),
                        });
                    }
                }
                Err(())
            })
            .await
            .ok()?;
        let search = self
            .request(
                ctx,
                session.base,
                "/subject-api/search/v2",
                Some(json!({"keyword":name,"page":1,"perPage":15,"subjectType":0}).to_string()),
                Some(&session.token),
            )
            .await?;
        let wanted_type = if media.season.is_some() { 2 } else { 1 };
        let item = search
            .get("results")?
            .as_array()?
            .iter()
            .filter_map(|group| group.get("subjects").and_then(Value::as_array))
            .flatten()
            .find(|item| {
                item.get("subjectType").and_then(Value::as_u64) == Some(wanted_type)
                    && item
                        .get("title")
                        .and_then(Value::as_str)
                        .is_some_and(|title| super::normalize(title) == super::normalize(name))
                    && year.is_none_or(|year| {
                        item.get("releaseDate")
                            .and_then(Value::as_str)
                            .is_some_and(|date| date.starts_with(&year.to_string()))
                    })
            })?;
        let subject = item.get("subjectId")?.as_str()?;
        let mut path = format!("/subject-api/play-info/v2?subjectId={subject}");
        if let Some(season) = media.season {
            use std::fmt::Write as _;
            let _ = write!(path, "&se={season}&ep={}", media.episode.unwrap_or(1));
        }
        let play = self
            .request(ctx, session.base, &path, None, Some(&session.token))
            .await?;
        Some(cards(
            &play,
            &super::display_title(name, year, media.season, media.episode),
        ))
    }
}

fn signature(
    key: &[u8],
    method: &str,
    url: &Url,
    body: Option<&str>,
    timestamp: &str,
) -> Option<String> {
    let mut query: Vec<_> = url.query_pairs().collect();
    query.sort_by(|a, b| a.0.cmp(&b.0));
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let path = format!(
        "{}{}{}",
        url.path(),
        if query.is_empty() { "" } else { "?" },
        query
    );
    let (length, digest) = body.map_or_else(
        || (String::new(), String::new()),
        |body| {
            (
                body.len().to_string(),
                hex(&Md5::digest(&body.as_bytes()[..body.len().min(102_400)])),
            )
        },
    );
    let canonical = format!(
        "{method}\napplication/json\napplication/json\n{length}\n{timestamp}\n{digest}\n{path}"
    );
    let mut mac = <Hmac<Md5> as Mac>::new_from_slice(key).ok()?;
    mac.update(canonical.as_bytes());
    Some(format!(
        "{timestamp}|2|{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The web URL can be a short upgrade notice. The signed cookie identifies the
/// real DASH manifest; never turn the notice into a successful media result.
fn manifest(cookie: &str) -> Option<Url> {
    for part in cookie.split(';').map(str::trim) {
        let resource = if let Some((_, rest)) = part.split_once("urlprefix=") {
            let token = rest.split(':').next()?;
            let raw = token.replace('-', "+").replace('_', "/");
            let padded = format!("{raw}{}", "=".repeat((4 - raw.len() % 4) % 4));
            String::from_utf8(STANDARD.decode(padded).ok()?).ok()?
        } else if let Some(policy) = part.strip_prefix("CloudFront-Policy=") {
            let raw = policy.replace('-', "+").replace('_', "=").replace('~', "/");
            let padded = format!("{raw}{}", "=".repeat((4 - raw.len() % 4) % 4));
            let value: Value = serde_json::from_slice(&STANDARD.decode(padded).ok()?).ok()?;
            value
                .pointer("/Statement/0/Resource")?
                .as_str()?
                .to_string()
        } else {
            continue;
        };
        let resource = resource.trim_end_matches('*').trim_end_matches('/');
        let url = Url::parse(&format!("{resource}/index.mpd")).ok()?;
        if url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".hakunaymatata.com"))
        {
            return Some(url);
        }
    }
    None
}

fn cards(data: &Value, title: &str) -> Vec<Stream> {
    let Some(entries) = data.get("streams").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .filter_map(|entry| {
            if entry.get("vipLocked").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let cookie = entry
                .get("signCookie")?
                .as_str()?
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let url = manifest(&cookie)?;
            if !seen.insert(url.clone()) {
                return None;
            }
            let mut stream = Stream::new(url, Format::Unknown)
                .with_label(format!("{title} (MovieBox adaptive)"))
                .with_ttl(Duration::from_mins(10));
            stream.meta.resolution = entry
                .get("resolutions")
                .or_else(|| data.get("displayResolutions"))
                .and_then(Value::as_str)
                .into_iter()
                .flat_map(|r| r.split(','))
                .filter_map(|r| r.trim().parse::<u16>().ok())
                .max();
            stream.meta.languages = vec![CountryCode::Multi];
            stream.meta.source_id = Some("moviebox".into());
            stream.meta.source_label = Some("MovieBox".into());
            stream.meta.request_headers = BTreeMap::from([
                ("Cookie".into(), cookie),
                ("User-Agent".into(), UA.into()),
                ("Referer".into(), REFERER.into()),
            ]);
            Some(stream)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signing_matches_an_independent_hmac_vector() {
        let url = Url::parse("https://api.example.test/path?z=1&a=two%20words")
            .unwrap_or_else(|e| panic!("URL: {e}"));
        assert_eq!(
            signature(&[1, 2, 3, 4], "POST", &url, Some("{}"), "1700000000123").as_deref(),
            Some("1700000000123|2|uMjMh3iS39xugWROJ0m4Qw==")
        );
    }
    #[test]
    fn signed_cookie_selects_dash_instead_of_the_upgrade_notice() {
        let prefix = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://sbcdn3.hakunaymatata.com/dash/title/");
        let cookie =
            format!("Edge-Cache-Cookie=urlprefix={prefix}:expires=9999999999:hmac=fixture;");
        let rows = cards(
            &json!({"streams":[{"url":"https://macdn.aoneroom.com/other/upgrade.mp4","signCookie":cookie,"resolutions":"480,1080,720"}]}),
            "Inception",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url.path(), "/dash/title/index.mpd");
        assert_eq!(rows[0].meta.resolution, Some(1080));
        assert!(rows[0].meta.request_headers.contains_key("Cookie"));
        assert!(
            cards(
                &json!({"streams":[{"url":"https://macdn.aoneroom.com/other/upgrade.mp4"}]}),
                "Inception"
            )
            .is_empty()
        );
        assert!(manifest("Edge-Cache-Cookie=invalid").is_none());
    }
    #[tokio::test]
    async fn mobile_pipeline_selects_the_exact_title_and_reuses_its_guest_session() {
        use async_trait::async_trait;
        use std::sync::{Arc, Mutex};
        use vsources_core::error::FetchError;
        use vsources_core::traits::{FetchResponse, Fetcher};
        use vsources_core::types::MediaId;
        struct Fixture {
            requests: Mutex<Vec<FetchRequest>>,
            cookie: String,
        }
        #[async_trait]
        impl Fetcher for Fixture {
            async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
                let body = match request.url.path() {
                    "/wefeed-mobile-bff/user-api/visitor-login" => {
                        assert_eq!(request.method, "POST");
                        json!({"data":{"token":"fixture-token"}})
                    }
                    "/wefeed-mobile-bff/subject-api/search/v2" => {
                        assert_eq!(
                            request.headers.get("Authorization").map(String::as_str),
                            Some("Bearer fixture-token")
                        );
                        json!({"data":{"results":[{"subjects":[
                            {"subjectId":"wrong","title":"Inception [Hindi]","subjectType":1,"releaseDate":"2010-01-01"},
                            {"subjectId":"right","title":"Inception","subjectType":1,"releaseDate":"2010-07-16"}
                        ]}]}})
                    }
                    "/wefeed-mobile-bff/subject-api/play-info/v2" => {
                        assert_eq!(request.url.query(), Some("subjectId=right"));
                        json!({"data":{"streams":[{"signCookie":self.cookie,"resolutions":"720,1080"}]}})
                    }
                    path => panic!("unexpected route: {path}"),
                };
                assert!(request.headers.contains_key("X-Tr-Signature"));
                let url = request.url.clone();
                self.requests
                    .lock()
                    .unwrap_or_else(|e| panic!("lock: {e}"))
                    .push(request);
                Ok(FetchResponse {
                    url,
                    status: 200,
                    headers: BTreeMap::new(),
                    body: body.to_string(),
                })
            }
        }
        let prefix = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://sbcdn3.hakunaymatata.com/dash/title/");
        let fetcher = Arc::new(Fixture {
            requests: Mutex::new(Vec::new()),
            cookie: format!("Edge-Cache-Cookie=urlprefix={prefix}:expires=9999999999:hmac=test"),
        });
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref(),
            media: None,
            source_id: None,
            referer: None,
        };
        let client = Client::new(vec![1; 30]);
        for _ in 0..2 {
            let rows = client
                .resolve(
                    &ctx,
                    &MediaRef::movie(MediaId::Tmdb(27205)),
                    "Inception",
                    Some(2010),
                )
                .await
                .unwrap_or_else(|| panic!("mobile resolve"));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].url.path(), "/dash/title/index.mpd");
        }
        let requests = fetcher
            .requests
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"));
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path().ends_with("visitor-login"))
                .count(),
            1
        );
    }
}
