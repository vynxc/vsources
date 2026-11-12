use super::*;

fn raw(format: Format, quality: &str, category: &str, url: &str) -> ReanimeStream {
    ReanimeStream {
        name: format!("ReAnime [fixture] | {quality} | {category}"),
        url: url.into(),
        quality: quality.into(),
        format,
        xor_key_b64: if format == Format::Hls {
            "fixture-key".into()
        } else {
            String::new()
        },
    }
}

fn card(format: Format, quality: &str, category: &str) -> Stream {
    build_cards(
        &[raw(
            format,
            quality,
            category,
            "https://media.example/opaque",
        )],
        "Johnny English (2003)",
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("fixture card"))
}

#[test]
fn ordinary_cards_do_not_invent_audio_or_technical_facts() {
    for format in [Format::Mp4, Format::Hls] {
        let value = card(format, "Auto", "sub");
        assert!(value.meta.languages.is_empty());
        assert!(value.meta.audio.is_empty());
        assert_eq!(value.meta.codec, None);
        assert_eq!(value.meta.quality, None);
        assert_eq!(value.meta.resolution, None);
        assert_eq!(value.meta.dubbed, None);
        assert_eq!(value.meta.subbed, None);
        assert!(
            !value
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Multi-audio")
                    || label.contains("Japanese (Sub)")
                    || label.contains("English (Dub)"))
        );
    }
}

#[test]
fn catalog_and_server_categories_do_not_declare_spoken_audio() {
    for category in ["dub", "sub", "multi", "English subtitles", "Japanese", ""] {
        let value = card(Format::Mp4, "1080p", category);
        assert!(value.meta.languages.is_empty(), "{category}");
        assert_eq!(value.meta.dubbed, None);
        assert_eq!(value.meta.subbed, None);
        assert!(
            value
                .label
                .as_deref()
                .is_some_and(|label| label.starts_with("Johnny English"))
        );
    }
}

#[test]
fn explicit_resolution_preserves_transport_identity_without_guessing_codecs() {
    for (quality, expected) in [
        ("720p", 720),
        ("1080P", 1080),
        ("4K", 2160),
        ("1920x800", 800),
        ("1440", 1440),
    ] {
        for format in [Format::Mp4, Format::Hls] {
            let value = card(format, quality, "sub");
            assert_eq!(value.meta.resolution, Some(expected), "{quality}");
            assert_eq!(value.meta.codec, None);
            assert_eq!(value.meta.quality, None);
            assert_eq!(value.url.as_str(), "https://media.example/opaque");
            assert_eq!(value.format, format);
            assert_eq!(
                value
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://flixcloud.cc/")
            );
            assert_eq!(
                value.meta.request_headers.get("Origin").map(String::as_str),
                Some(FLIXCLOUD)
            );
            assert_eq!(value.meta.source_id.as_deref(), Some(ID));
            assert_eq!(value.ttl, TTL);
            assert_eq!(
                value
                    .behavior_hints
                    .get("reanimeXorKey")
                    .map(String::as_str),
                (format == Format::Hls).then_some("fixture-key")
            );
        }
    }
}

#[test]
fn missing_or_unrecognized_quality_does_not_become_1080() {
    for quality in ["", "Auto", "unknown", "Server2023", "0", "70000"] {
        assert_eq!(
            card(Format::Mp4, quality, "dub").meta.resolution,
            None,
            "{quality}"
        );
    }
}

#[test]
fn genuine_filename_facts_can_fill_fields_after_provider_mapping() {
    let name = "Fixture.720p.BluRay.x265.DTS-HD.MA.5.1.mkv";
    let mut value = build_cards(
        &[raw(
            Format::Mp4,
            "Auto",
            "sub",
            &format!("https://media.example/{name}"),
        )],
        "Johnny English",
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("fixture card"));
    vsources_core::enrich::enrich_stream(&mut value, name);
    assert_eq!(value.meta.resolution, Some(720));
    assert_eq!(value.meta.codec.as_deref(), Some("x265"));
    assert_eq!(value.meta.quality.as_deref(), Some("BluRay"));
    assert!(value.meta.audio.iter().any(|audio| audio == "DTS Lossless"));
}

#[test]
fn missing_download_resolution_remains_unknown() {
    let data = serde_json::json!({"nodes":[{"data":[
        {"video":1,"download":3},{"fileId":2},"fixture-id",{ "base":4,"token":5 },"https://media.example","fixture-token"
    ]}]});
    let (url, quality) = download_from_data(&data).unwrap_or_else(|| panic!("fixture download"));
    assert!(quality.is_empty());
    let value = build_cards(
        &[raw(Format::Mp4, &quality, "dub", url.as_str())],
        "Fixture",
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("fixture card"));
    assert_eq!(value.meta.resolution, None);
}

#[tokio::test]
async fn legacy_hls_requires_real_resolution_evidence() -> Result<(), SourceError> {
    let mock = Arc::new(pages(ScriptedFetcher::default()));
    let value = provider(&mock)
        .resolve(
            &ctx_for(&mock),
            &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1),
        )
        .await?;
    assert_eq!(value.len(), 1);
    assert_eq!(value[0].format, Format::Hls);
    assert_eq!(value[0].meta.resolution, None);
    assert!(value[0].meta.languages.is_empty());
    assert!(
        !value[0]
            .label
            .as_deref()
            .is_some_and(|label| label.contains("1080p"))
    );
    assert!(value[0].behavior_hints.contains_key("reanimeXorKey"));
    Ok(())
}

pub(super) fn assert_verified_english_metadata(value: &Stream, resolution: Option<&str>) {
    assert_eq!(
        value.meta.audio_selection,
        Some(AudioSelection {
            language: CountryCode::En,
            audio_index: 1
        })
    );
    assert_eq!(value.meta.dubbed, Some(true));
    assert_eq!(value.meta.languages, vec![CountryCode::En]);
    assert_eq!(value.meta.resolution, resolution.map(|_| 1080));
    if resolution.is_none() {
        assert!(
            !value
                .label
                .as_deref()
                .is_some_and(|label| label.contains("1080"))
        );
    }
}
