use super::*;

async fn mp4(label: &str) -> Stream {
    let mock = Arc::new(ScriptedFetcher::default());
    let ctx = ctx_for(&mock);
    let source: ApiSource = serde_json::from_value(
        serde_json::json!({"url":"https://media.example/movie.mp4","quality":label,"format":"mp4"}),
    )
    .unwrap_or_else(|error| panic!("fixture: {error}"));
    let cards = resolve_source(&ctx, "fixture", source).await;
    wrapper_stream(
        &cards[0],
        &Url::parse(&cards[0].url).unwrap_or_else(|error| panic!("fixture URL: {error}")),
        &parse_language(&cards[0].language),
        "Fixture",
    )
}

#[tokio::test]
async fn unknown_audio_is_not_english_or_an_audio_codec() {
    let stream = mp4("Auto").await;
    assert!(stream.meta.languages.is_empty());
    assert!(stream.meta.audio.is_empty());
    assert!(
        !stream
            .label
            .as_deref()
            .is_some_and(|label| label.contains("English"))
    );
}

#[tokio::test]
async fn unknown_technical_properties_remain_unknown() {
    let stream = mp4("Auto").await;
    assert_eq!(stream.meta.resolution, None);
    assert_eq!(stream.meta.codec, None);
    assert_eq!(stream.meta.quality, None);
    let four_k = mp4("4K").await;
    assert_eq!(four_k.meta.resolution, Some(2160));
    assert_eq!(
        four_k.meta.codec, None,
        "resolution does not establish video codec"
    );
}

#[tokio::test]
async fn declared_languages_are_not_audio_codecs_or_default_english() {
    for (label, expected) in [
        ("720p | Hindi", vec![CountryCode::Hi]),
        ("720p | English", vec![CountryCode::En]),
        ("720p | Multi Audio", vec![CountryCode::Multi]),
        ("720p | mystery", vec![]),
        ("720p | Not English", vec![]),
        ("720p | No English", vec![]),
        ("720p | No English, Hindi", vec![]),
        ("720p | Hindi/No English", vec![]),
        ("720p | no,en", vec![CountryCode::No, CountryCode::En]),
        ("720p | Hindi + No English", vec![]),
        ("720p | no", vec![CountryCode::No]),
        ("720p | English subtitles", vec![]),
        ("720p | French", vec![CountryCode::Fr]),
    ] {
        let stream = mp4(label).await;
        assert_eq!(stream.meta.languages, expected, "{label}");
        assert!(
            stream.meta.audio.is_empty(),
            "language must not become a codec"
        );
    }
}

#[tokio::test]
async fn master_manifest_codecs_do_not_follow_quality_tier() {
    let mock = Arc::new(ScriptedFetcher::default().page("/master.m3u8", 200,
        "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=3840x2160,CODECS=\"avc1.640033,mp4a.40.2\"\n/4k.m3u8\n#EXT-X-STREAM-INF:RESOLUTION=1920x800\n/cropped.m3u8\n"));
    let ctx = ctx_for(&mock);
    let source: ApiSource = serde_json::from_value(serde_json::json!({"url":"https://media.example/master.m3u8","quality":"Auto","format":"hls"})).unwrap_or_else(|error| panic!("fixture: {error}"));
    let cards = resolve_source(&ctx, "fixture", source).await;
    let streams: Vec<_> = cards
        .iter()
        .map(|card| {
            wrapper_stream(
                card,
                &Url::parse(&card.url).unwrap_or_else(|error| panic!("fixture URL: {error}")),
                &parse_language(&card.language),
                "Fixture",
            )
        })
        .collect();
    assert_eq!(streams[0].meta.codec.as_deref(), Some("AVC"));
    assert_eq!(streams[0].meta.audio, vec!["AAC"]);
    assert_eq!(
        streams[1].meta.resolution,
        Some(1080),
        "existing cropped-film quality tier is preserved"
    );
    assert_eq!(streams[1].meta.codec, None);
    assert!(
        streams
            .iter()
            .all(|stream| stream.meta.languages.is_empty())
    );
}

#[test]
fn codec_hints_require_an_actual_unambiguous_attribute() {
    assert_eq!(
        manifest_hints(r#"#EXT-X-STREAM-INF:CODECS="avc1.640028,hvc1.1.6.L93.B0""#).codec,
        None
    );
    assert_eq!(
        manifest_hints(r#"#EXT-X-STREAM-INF:NAME="CODECS=avc1.640028""#).codec,
        None
    );
    assert_eq!(
        manifest_hints(r#"#EXT-X-STREAM-INF:CODECS="avc1.640028"#).codec,
        None
    );
    assert!(
        manifest_hints("#EXT-X-STREAM-INF:RESOLUTION=3840x2160")
            .audio
            .is_empty()
    );
}

#[test]
fn external_rendition_codecs_are_not_claimed_on_detached_child() {
    let hints = manifest_hints(
        r#"#EXT-X-STREAM-INF:RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2",AUDIO="external""#,
    );
    assert_eq!(hints.codec.as_deref(), Some("AVC"));
    assert!(hints.audio.is_empty());
    assert!(
        manifest_hints(r#"#EXT-X-STREAM-INF:CODECS="mp4a.40.2",AUDIO=unknown"#)
            .audio
            .is_empty()
    );
}

#[tokio::test]
async fn equivalent_mp4_quality_labels_preserve_source_order() -> Result<(), SourceError> {
    let mock = Arc::new(tmdb_mock().page("/api/provider", 200,
        r#"{"data":{"sources":[{"quality":"1080p","url":"https://media.example/plain.mp4","format":"mp4"},{"quality":"1080p | English","url":"https://media.example/labelled.mp4","format":"mp4"}]}}"#));
    let source = provider(&mock);
    let ctx = ctx_for(&mock);
    let streams = source
        .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
        .await?;
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].url.path(), "/plain.mp4");
    assert_eq!(streams[1].url.path(), "/labelled.mp4");
    Ok(())
}

#[tokio::test]
async fn final_quality_order_prefers_known_high_quality_and_stable_equal_tiers()
-> Result<(), SourceError> {
    let values = [
        ("Auto", "unknown"),
        ("360p", "360"),
        ("1080p | English", "1080-labelled"),
        ("4K", "4k"),
        ("720p", "720"),
        ("1080p", "1080"),
        ("1440p", "1440"),
    ];
    let sources: Vec<_> = values.into_iter().map(|(quality, name)| serde_json::json!({"quality":quality,"url":format!("https://media.example/{name}.mp4"),"format":"mp4"})).collect();
    let mock = Arc::new(tmdb_mock().page(
        "/api/provider",
        200,
        serde_json::json!({"data":{"sources":sources}}).to_string(),
    ));
    let source = provider(&mock);
    let ctx = ctx_for(&mock);
    let streams = source
        .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
        .await?;
    assert_eq!(
        streams
            .iter()
            .map(|stream| stream.url.path())
            .collect::<Vec<_>>(),
        vec![
            "/4k.mp4",
            "/1440.mp4",
            "/1080-labelled.mp4",
            "/1080.mp4",
            "/720.mp4",
            "/360.mp4",
            "/unknown.mp4"
        ]
    );
    Ok(())
}
