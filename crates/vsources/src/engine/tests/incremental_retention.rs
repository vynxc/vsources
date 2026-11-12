use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

struct Cancelled(Arc<AtomicBool>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct IncrementalFixture {
    info: SourceInfo,
    early: Vec<Stream>,
    final_streams: Vec<Stream>,
    delay: Duration,
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl Source for IncrementalFixture {
    fn info(&self) -> &SourceInfo {
        &self.info
    }
    async fn resolve(&self, _: &ResolveCtx<'_>, _: &MediaRef) -> Result<Vec<Stream>, SourceError> {
        tokio::time::sleep(self.delay).await;
        Ok(self.final_streams.clone())
    }
    async fn resolve_incremental(
        &self,
        _: &ResolveCtx<'_>,
        _: &MediaRef,
        publish: &(dyn Fn(Vec<Stream>) + Send + Sync),
    ) -> Result<Vec<Stream>, SourceError> {
        let _cancelled = Cancelled(self.cancelled.clone());
        publish(self.early.clone());
        tokio::time::sleep(self.delay).await;
        Ok(self.final_streams.clone())
    }
}

fn item(path: &str, label: &str) -> Stream {
    let mut value = Stream::new(
        test_url(&format!("https://media.example/{path}.mp4")),
        Format::Mp4,
    );
    value.label = Some(label.into());
    value.meta.resolution = Some(720);
    value
}

#[tokio::test(start_paused = true)]
async fn default_cached_discovery_keeps_early_results_at_eighteen_second_timeout() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let source = Arc::new(vsources_providers::CachedSource::new(Arc::new(
        IncrementalFixture {
            info: info("incremental", 0),
            early: vec![item("fast", "fast")],
            final_streams: vec![item("fast", "fast")],
            delay: Duration::from_secs(60),
            cancelled: cancelled.clone(),
        },
    )));
    let engine = test_builder()
        .sources(vec![source])
        .stream_limit(8)
        .per_source_timeout(Duration::from_secs(18))
        .probe_config(ProbeConfig {
            enabled: false,
            ..ProbeConfig::default()
        })
        .build()
        .unwrap_or_else(|e| panic!("fixture: {e}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reference = media();
    let resolving = engine.resolve_progressive(&reference, tx);
    tokio::pin!(resolving);
    let early = tokio::select! {
        batch = rx.recv() => batch.unwrap_or_else(|| panic!("early batch missing")),
        result = &mut resolving => panic!("source completed before callback: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => panic!("default discovery withheld the fast callback"),
    };
    assert_eq!(early[0].url.path(), "/fast.mp4");
    let result = resolving
        .await
        .unwrap_or_else(|e| panic!("partial result lost at timeout: {e}"));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].url, early[0].url);
    assert!(
        cancelled.load(Ordering::SeqCst),
        "provider future must be dropped"
    );
}

fn fixture(
    id: &str,
    priority: i32,
    early: Vec<Stream>,
    final_streams: Vec<Stream>,
    delay: u64,
) -> Arc<dyn Source> {
    Arc::new(vsources_providers::CachedSource::new(Arc::new(
        IncrementalFixture {
            info: info(id, priority),
            early,
            final_streams,
            delay: Duration::from_secs(delay),
            cancelled: Arc::new(AtomicBool::new(false)),
        },
    )))
}

fn without_probes(sources: Vec<Arc<dyn Source>>) -> Engine {
    test_builder()
        .sources(sources)
        .probe_config(ProbeConfig {
            enabled: false,
            ..ProbeConfig::default()
        })
        .build()
        .unwrap_or_else(|error| panic!("fixture: {error}"))
}

#[tokio::test(start_paused = true)]
async fn authoritative_final_replaces_callback_metadata_and_order_for_both_paths() {
    for selected in [false, true] {
        let engine = without_probes(vec![fixture(
            "incremental",
            0,
            vec![
                item("a", "provisional a"),
                item("removed", "withdrawn"),
                item("b", "provisional b"),
            ],
            vec![item("b", "final b"), item("a", "final a")],
            2,
        )]);
        let reference = media();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let resolving = async {
            if selected {
                engine
                    .resolve_provider_progressive(&reference, "incremental", None, None, tx)
                    .await
            } else {
                engine.resolve_progressive(&reference, tx).await
            }
        };
        tokio::pin!(resolving);
        let early = tokio::select! {
            batch = rx.recv() => batch.unwrap_or_else(|| panic!("expected early view")),
            result = &mut resolving => panic!("must publish before final: {result:?}"),
        };
        assert!(early.iter().any(|stream| {
            stream
                .label
                .as_deref()
                .is_some_and(|label| label.starts_with("provisional"))
        }));
        let final_streams = resolving
            .await
            .unwrap_or_else(|error| panic!("fixture: {error}"));
        assert_eq!(
            final_streams
                .iter()
                .map(|stream| stream.label.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("final b"), Some("final a")]
        );
        if !selected {
            let mut last = early;
            while let Ok(snapshot) = rx.try_recv() {
                last = snapshot;
            }
            assert_eq!(last, final_streams);
        }
        let warm = engine
            .resolve(&reference)
            .await
            .unwrap_or_else(|error| panic!("warm cache: {error}"));
        assert_eq!(warm, final_streams);
    }
}

#[tokio::test(start_paused = true)]
async fn empty_authoritative_final_withdraws_early_view() {
    let engine = without_probes(vec![fixture(
        "incremental",
        0,
        vec![item("removed", "provisional")],
        vec![],
        2,
    )]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = engine
        .resolve_progressive(&media(), tx)
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert!(result.is_empty());
    let mut saw_early = false;
    let mut last = None;
    while let Ok(snapshot) = rx.try_recv() {
        saw_early |= !snapshot.is_empty();
        last = Some(snapshot);
    }
    assert!(saw_early);
    assert_eq!(last, Some(vec![]));
}

#[tokio::test(start_paused = true)]
async fn provider_priority_context_identity_and_caps_survive_provisional_takeover() {
    let mut alternate_header = item("shared", "header");
    alternate_header
        .meta
        .request_headers
        .insert("Referer".into(), "https://other.example/".into());
    let mut alternate_audio = item("shared", "selected");
    alternate_audio.meta.audio_selection = Some(vsources_core::types::AudioSelection {
        language: CountryCode::En,
        audio_index: 1,
    });
    let engine = test_builder()
        .sources(vec![
            fixture(
                "priority",
                10,
                vec![],
                vec![
                    item("shared", "priority final"),
                    alternate_header.clone(),
                    alternate_audio.clone(),
                    item("over-cap", "excluded"),
                ],
                2,
            ),
            fixture(
                "fast",
                0,
                vec![item("shared", "fast provisional")],
                vec![item("shared", "fast final")],
                1,
            ),
        ])
        .stream_limit(3)
        .probe_config(ProbeConfig {
            enabled: false,
            ..ProbeConfig::default()
        })
        .build()
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = engine
        .resolve_progressive(&media(), tx)
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].label.as_deref(), Some("priority final"));
    assert!(
        result
            .iter()
            .all(|stream| stream.meta.source_id.as_deref() == Some("priority"))
    );
    assert_eq!(
        result[1].meta.request_headers,
        alternate_header.meta.request_headers
    );
    assert_eq!(
        result[2].meta.audio_selection,
        alternate_audio.meta.audio_selection
    );
    while let Ok(snapshot) = rx.try_recv() {
        assert!(snapshot.len() <= 3);
    }
}

#[tokio::test(start_paused = true)]
async fn preferred_english_does_not_impose_strict_dub_requirement() {
    let mut unknown = item("unknown", "unknown");
    unknown.meta.languages = vec![CountryCode::En];
    let engine = without_probes(vec![fixture(
        "incremental",
        0,
        vec![unknown.clone()],
        vec![unknown],
        1,
    )]);
    let (tx, _) = mpsc::unbounded_channel();
    let preferred = engine
        .resolve_provider_progressive(&media(), "incremental", Some("en"), None, tx)
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert_eq!(preferred.len(), 1);
    assert!(
        engine
            .resolve_english_dub(&media())
            .await
            .unwrap_or_else(|error| panic!("fixture: {error}"))
            .is_empty()
    );
}

struct FailedIncremental(SourceInfo);
#[async_trait]
impl Source for FailedIncremental {
    fn info(&self) -> &SourceInfo {
        &self.0
    }
    async fn resolve(&self, _: &ResolveCtx<'_>, _: &MediaRef) -> Result<Vec<Stream>, SourceError> {
        Err(SourceError::scrape(&self.0.id, "fixture failure"))
    }
    async fn resolve_incremental(
        &self,
        _: &ResolveCtx<'_>,
        _: &MediaRef,
        publish: &(dyn Fn(Vec<Stream>) + Send + Sync),
    ) -> Result<Vec<Stream>, SourceError> {
        publish(vec![item("dead", "dead"), item("usable", "usable")]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        Err(SourceError::scrape(&self.0.id, "fixture failure"))
    }
}

struct CallbackProbe;
#[async_trait]
impl Fetcher for CallbackProbe {
    async fn request(
        &self,
        _: vsources_core::traits::FetchRequest,
    ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError> {
        panic!("bounded probes only")
    }
    async fn probe(
        &self,
        request: vsources_core::traits::FetchRequest,
        _: usize,
    ) -> Result<Option<vsources_core::traits::ProbeResponse>, vsources_core::error::FetchError>
    {
        Ok(Some(vsources_core::traits::ProbeResponse {
            status: if request.url.path() == "/dead.mp4" {
                404
            } else {
                403
            },
            url: request.url,
            headers: BTreeMap::new(),
            body: Vec::new(),
            truncated: false,
        }))
    }
}

#[tokio::test(start_paused = true)]
async fn failed_provider_retains_only_validated_callbacks_in_both_paths() {
    for selected in [false, true] {
        let source = Arc::new(vsources_providers::CachedSource::new(Arc::new(
            FailedIncremental(info("incremental", 0)),
        )));
        let engine = test_builder()
            .fetcher(Arc::new(CallbackProbe))
            .sources(vec![source])
            .build()
            .unwrap_or_else(|error| panic!("fixture: {error}"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = if selected {
            engine
                .resolve_provider_progressive(&media(), "incremental", None, None, tx)
                .await
        } else {
            engine.resolve_progressive(&media(), tx).await
        }
        .unwrap_or_else(|error| panic!("validated partial lost: {error}"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].url.path(), "/usable.mp4");
        while let Ok(batch) = rx.try_recv() {
            assert!(
                batch
                    .iter()
                    .all(|stream| stream.url.path() == "/usable.mp4")
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn caller_cancellation_drops_upstream_after_early_publication() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let source = Arc::new(vsources_providers::CachedSource::new(Arc::new(
        IncrementalFixture {
            info: info("incremental", 0),
            early: vec![item("fast", "fast")],
            final_streams: vec![],
            delay: Duration::from_secs(60),
            cancelled: cancelled.clone(),
        },
    )));
    let engine = without_probes(vec![source]);
    let reference = media();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut resolving = Box::pin(engine.resolve_progressive(&reference, tx));
    tokio::select! {
        batch = rx.recv() => { assert!(batch.is_some()); }
        result = &mut resolving => panic!("must remain pending: {result:?}"),
    }
    drop(resolving);
    assert!(cancelled.load(Ordering::SeqCst));
}

struct TimedProbe;
#[async_trait]
impl Fetcher for TimedProbe {
    async fn request(
        &self,
        _: vsources_core::traits::FetchRequest,
    ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError> {
        panic!("bounded probes only")
    }
    async fn probe(
        &self,
        request: vsources_core::traits::FetchRequest,
        _: usize,
    ) -> Result<Option<vsources_core::traits::ProbeResponse>, vsources_core::error::FetchError>
    {
        tokio::time::sleep(Duration::from_secs(
            if request.url.path() == "/removed.mp4" {
                60
            } else {
                3
            },
        ))
        .await;
        Ok(None)
    }
}

#[tokio::test(start_paused = true)]
async fn final_validation_overlaps_pending_provisional_validation() {
    let engine = test_builder()
        .fetcher(Arc::new(TimedProbe))
        .sources(vec![fixture(
            "incremental",
            0,
            vec![item("removed", "provisional")],
            vec![item("final", "authoritative")],
            1,
        )])
        .build()
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    let started = tokio::time::Instant::now();
    let result = engine
        .resolve(&media())
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].url.path(), "/final.mp4");
    assert!(
        started.elapsed() <= Duration::from_secs(5),
        "final validation must not queue behind an unrelated provisional probe: {:?}",
        started.elapsed()
    );
}

struct MixedLanguage(SourceInfo);
#[async_trait]
impl Source for MixedLanguage {
    fn info(&self) -> &SourceInfo {
        &self.0
    }
    async fn resolve(&self, _: &ResolveCtx<'_>, _: &MediaRef) -> Result<Vec<Stream>, SourceError> {
        let mut french = item("french", "French");
        french.meta.languages = vec![CountryCode::Fr];
        let mut english = item("english", "English");
        english.meta.languages = vec![CountryCode::En];
        english.meta.dubbed = Some(true);
        Ok(vec![french, english])
    }
    async fn resolve_english_dub(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        self.resolve(ctx, media).await
    }
}

#[tokio::test(start_paused = true)]
async fn strict_english_filters_before_limit_but_disabled_probe_preference_is_not_strict() {
    let engine = test_builder()
        .sources(vec![Arc::new(MixedLanguage(info("incremental", 0)))])
        .stream_limit(1)
        .probe_config(ProbeConfig {
            enabled: false,
            ..ProbeConfig::default()
        })
        .build()
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    let strict = engine
        .resolve_english_dub(&media())
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert_eq!(
        strict.len(),
        1,
        "non-English candidates must not consume the strict budget"
    );
    assert_eq!(strict[0].url.path(), "/english.mp4");
    let (tx, _) = mpsc::unbounded_channel();
    let preferred = engine
        .resolve_provider_progressive(&media(), "incremental", Some("en"), None, tx)
        .await
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    assert_eq!(preferred.len(), 1);
    assert_eq!(preferred[0].url.path(), "/french.mp4");
}
