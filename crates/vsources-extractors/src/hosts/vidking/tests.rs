use super::*;
use crate::testing::{ScriptedFetcher, ctx_for};
use serde_json::json;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap_or_else(|e| panic!("test URL: {e}"))
}

#[tokio::test]
async fn real_registry_media_fallback_decrypts_and_coalesces_across_embeds()
-> Result<(), ExtractorError> {
    // Independently generated Node fixture, also used by the protocol tests.
    let payload = "dfS4GATTVMdIPIt14QZYTraUVVwxDX_-BD_Vx8QsdCkZo0M7i3nsqCa6IL5M7qFN0EPpGRiD69vMbws-4I5m6Rl8eeRysj30vEp6ukKnljea0adax-w9CXxFcuFgFpligneO6dT_AtmPeMsVpcmiPEYIStUqC-muhjObHMzlCVpJahcxBCgW7QESSv5Ykr3V07PhypGm2uyF-RK5Ct2QcA";
    let fetcher = ScriptedFetcher::default()
        .page("/seed", r#"{"seed":"a1b2c3d4e5f6a7b8"}"#)
        .page("/cdn/sources-with-title", payload);
    let mut ctx = ctx_for(&fetcher, None);
    ctx.media = Some(ResolvedMedia {
        tmdb_id: Some(123_456),
        imdb_id: None,
        name: "Test Movie".into(),
        year: Some(2026),
        season: None,
        episode: None,
    });
    ctx.source_id = Some("vidfast");
    let registry = crate::ExtractorRegistry::new(vec![Arc::new(VidKing::new())]);
    let first = url("https://unknown.example/a");
    let second = url("https://unknown.example/b");
    let (a, b) = tokio::join!(
        registry.extract(&ctx, &first),
        registry.extract(&ctx, &second)
    );
    let a = a?;
    let b = b?;
    assert_eq!(a.len(), 1);
    assert_eq!(a, b);
    assert_eq!(a[0].meta.resolution, Some(1080));
    assert_eq!(a[0].meta.source_id.as_deref(), Some("vidfast"));
    assert_eq!(
        a[0].meta.request_headers.get("Referer").map(String::as_str),
        Some("https://www.vidking.net/")
    );
    assert_eq!(
        fetcher
            .requests()
            .iter()
            .filter(|r| r.url.path() == "/cdn/sources-with-title")
            .count(),
        1
    );
    ctx.source_id = Some("vidsrcsbs");
    let c = registry.extract(&ctx, &first).await?;
    assert_eq!(c[0].meta.source_id.as_deref(), Some("vidsrcsbs"));
    assert_eq!(
        fetcher
            .requests()
            .iter()
            .filter(|r| r.url.path() == "/cdn/sources-with-title")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn vimeos_omits_referer_and_other_hosts_keep_it() {
    let payload = json!({"sources":[
        {"url":"https://s9.vimeos.net/master.m3u8","quality":"1080p English"},
        {"url":"https://moon.peakstorm.top/master.m3u8","quality":"2160p"},
        {"url":"https://s9.vimeos.net/master.m3u8","quality":"1080p"}
    ]});
    let streams = assemble(&[(speedracelight::PROVIDERS[0], Some(payload))]);
    assert_eq!(streams.len(), 2);
    assert!(streams[0].meta.request_headers.is_empty());
    assert_eq!(streams[0].meta.languages, vec![CountryCode::En]);
    assert!(streams[1].meta.request_headers.contains_key("Referer"));
}

#[test]
fn parses_movie_and_episode_embeds_and_rejects_lookalike_hosts() {
    let fetcher = ScriptedFetcher::default();
    let ctx = ctx_for(&fetcher, None);
    let extractor = VidKing::new();
    assert!(extractor.supports(&ctx, &url("https://www.vidking.net/embed/movie/27205")));
    assert!(extractor.supports(&ctx, &url("https://vidking.net/embed/tv/1396/1/2")));
    assert!(!extractor.supports(&ctx, &url("https://evilvidking.net/embed/movie/27205")));
    assert!(!extractor.supports(&ctx, &url("https://vidking.net/embed/tv/1396")));
    let media = embed_media(&url("https://vidking.net/embed/tv/1396/1/2"))
        .unwrap_or_else(|| panic!("episode"));
    assert_eq!(media.season, Some(1));
    assert_eq!(media.episode, Some(2));
}
