use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct Active<'a>(&'a AtomicUsize);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct PendingDirect {
    inner: Arc<ScriptedFetcher>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait]
impl Fetcher for PendingDirect {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        if request.url.path() == "/api/stream" {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let _active = Active(&self.active);
            return std::future::pending().await;
        }
        self.inner.request(request).await
    }
}

struct DelayedSub {
    active: AtomicUsize,
    peak: AtomicUsize,
}

struct TwoSlowSubs(DelayedSub);

#[async_trait]
impl Extractor for TwoSlowSubs {
    fn id(&self) -> &'static str {
        "two-slow-subs"
    }
    fn label(&self) -> &'static str {
        "TwoSlowSubs"
    }
    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        matches!(url.host_str(), Some("megaplay.buzz" | "vidnest.fun"))
    }
    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        self.0.extract(ctx, url).await
    }
}

#[async_trait]
impl Extractor for DelayedSub {
    fn id(&self) -> &'static str {
        "delayed-sub"
    }

    fn label(&self) -> &'static str {
        "DelayedSub"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str() == Some("megaplay.buzz")
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _active = Active(&self.active);
        if url.path().ends_with("/sub") {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        StubExtractor.extract(ctx, url).await
    }
}

fn resolved() -> ResolvedMedia {
    ResolvedMedia {
        tmdb_id: Some(TMDB_ID),
        imdb_id: None,
        name: "Solo Leveling".to_string(),
        year: Some(2024),
        season: Some(1),
        episode: Some(2),
    }
}

fn anime_mock() -> Arc<ScriptedFetcher> {
    Arc::new(ScriptedFetcher::default().page(
        "/api/anime/search",
        200,
        r#"{"results":[{"id":21,"title":"Solo Leveling"}]}"#,
    ))
}

struct SharedFour {
    inner: Arc<ScriptedFetcher>,
    permits: tokio::sync::Semaphore,
    direct_active: AtomicUsize,
    direct_peak: AtomicUsize,
}

#[async_trait]
impl Fetcher for SharedFour {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .unwrap_or_else(|_| panic!("fixture remains open"));
        if request.url.path() == "/api/stream" {
            let active = self.direct_active.fetch_add(1, Ordering::SeqCst) + 1;
            self.direct_peak.fetch_max(active, Ordering::SeqCst);
            let _active = Active(&self.direct_active);
            return std::future::pending().await;
        }
        self.inner.request(request).await
    }
}

struct NetworkAnime;

#[async_trait]
impl Extractor for NetworkAnime {
    fn id(&self) -> &'static str {
        "network-anime"
    }
    fn label(&self) -> &'static str {
        "NetworkAnime"
    }
    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str() == Some("megaplay.buzz")
    }
    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        ctx.fetcher.request(FetchRequest::get(url.clone())).await?;
        StubExtractor.extract(ctx, url).await
    }
}

#[tokio::test(start_paused = true)]
async fn shared_four_permits_admit_anime_then_restore_direct_capacity() {
    let mock = Arc::new(
        ScriptedFetcher::default()
            .page(
                "/api/anime/search",
                200,
                r#"{"results":[{"id":21,"title":"Solo Leveling"}]}"#,
            )
            .page("/stream/ani/21/2/sub", 200, "sub")
            .page("/stream/ani/21/2/dub", 200, "dub"),
    );
    let fetcher = SharedFour {
        inner: mock.clone(),
        permits: tokio::sync::Semaphore::new(4),
        direct_active: AtomicUsize::new(0),
        direct_peak: AtomicUsize::new(0),
    };
    let provider = StreamXTV::new(
        Arc::new(TmdbClient::new("test-key", mock.clone())),
        Arc::new(ExtractorRegistry::new(vec![Arc::new(NetworkAnime)])),
    );
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let batches = Mutex::new(Vec::new());
    let publish = |batch: Vec<Stream>| {
        assert!(
            fetcher.direct_active.load(Ordering::SeqCst) <= 2,
            "direct work reserves capacity until anime publication completes"
        );
        batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch);
    };
    let media = MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(18),
            provider.resolve_incremental(&ctx, &media, &publish)
        )
        .await
        .is_err()
    );
    assert_eq!(
        batches.lock().unwrap_or_else(PoisonError::into_inner).len(),
        2,
        "shared shipping transport must admit lookup and both embed fetches"
    );
    assert_eq!(mock.hits("/stream/ani/21/2/sub"), 1);
    assert_eq!(mock.hits("/stream/ani/21/2/dub"), 1);
    assert_eq!(
        fetcher.direct_peak.load(Ordering::SeqCst),
        4,
        "direct work regains the full shared capacity after anime completes"
    );
    assert_eq!(fetcher.direct_active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn absent_anime_restores_full_shared_direct_capacity() {
    let mock =
        Arc::new(ScriptedFetcher::default().page("/api/anime/search", 200, r#"{"results":[]}"#));
    let fetcher = SharedFour {
        inner: mock.clone(),
        permits: tokio::sync::Semaphore::new(4),
        direct_active: AtomicUsize::new(0),
        direct_peak: AtomicUsize::new(0),
    };
    let provider = provider(&mock);
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let media = MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(18),
            provider.resolve_incremental(&ctx, &media, &|_| panic!(
                "no anime match publishes nothing"
            ))
        )
        .await
        .is_err()
    );
    assert_eq!(fetcher.direct_peak.load(Ordering::SeqCst), 4);
    assert_eq!(fetcher.direct_active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn anime_publishes_before_caller_deadline_and_cancels_direct_work() {
    let mock = anime_mock();
    let fetcher = PendingDirect {
        inner: mock.clone(),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let provider = provider(&mock);
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let batches = Mutex::new(Vec::new());
    let started = tokio::time::Instant::now();
    let publish = |batch: Vec<Stream>| {
        batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((started.elapsed(), batch));
    };
    let media = MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2);
    let result = tokio::time::timeout(
        Duration::from_secs(18),
        provider.resolve_incremental(&ctx, &media, &publish),
    )
    .await;
    assert!(result.is_err(), "pending direct branch must be cancelled");
    let batches = batches.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(batches.len(), 2, "ready sub and dub survive cancellation");
    assert!(
        batches
            .iter()
            .all(|(time, _)| *time < Duration::from_secs(18))
    );
    assert_eq!(batches[1].1[0].meta.dubbed, Some(true));
    assert!(batches[1].1[0].meta.languages.contains(&CountryCode::En));
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher.peak.load(Ordering::SeqCst), BATCH_SIZE);
}

#[tokio::test(start_paused = true)]
async fn slow_sub_does_not_block_dub_publication_and_embed_work_is_bounded() {
    let mock = anime_mock();
    let fetcher = PendingDirect {
        inner: mock.clone(),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let extractor = Arc::new(DelayedSub {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let provider = StreamXTV::new(
        Arc::new(TmdbClient::new("test-key", mock)),
        Arc::new(ExtractorRegistry::new(vec![extractor.clone()])),
    );
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let batches = Mutex::new(Vec::new());
    let publish = |batch| {
        batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch);
    };
    let media = MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(18),
            provider.resolve_incremental(&ctx, &media, &publish),
        )
        .await
        .is_err()
    );
    let batches = batches.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        batches.len(),
        1,
        "ready dub is published while sub is pending"
    );
    assert_eq!(batches[0][0].meta.dubbed, Some(true));
    assert_eq!(extractor.peak.load(Ordering::SeqCst), 2);
    assert_eq!(extractor.active.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn two_slow_sub_providers_cannot_occupy_the_ready_dub_lane() {
    let mock = anime_mock();
    let fetcher = PendingDirect {
        inner: mock.clone(),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let extractor = Arc::new(TwoSlowSubs(DelayedSub {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    }));
    let provider = StreamXTV::new(
        Arc::new(TmdbClient::new("test-key", mock)),
        Arc::new(ExtractorRegistry::new(vec![extractor.clone()])),
    );
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let batches = Mutex::new(Vec::new());
    let publish = |batch: Vec<Stream>| {
        batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch);
    };
    let media = MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(18),
            provider.resolve_incremental(&ctx, &media, &publish)
        )
        .await
        .is_err()
    );
    let batches = batches.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        batches.len(),
        2,
        "both ready dub providers publish while sub work remains pending"
    );
    assert!(
        batches
            .iter()
            .all(|batch| batch[0].meta.dubbed == Some(true))
    );
    assert_eq!(extractor.0.peak.load(Ordering::SeqCst), 2);
    assert_eq!(extractor.0.active.load(Ordering::SeqCst), 0);
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn final_results_keep_direct_then_canonical_anime_order_after_out_of_order_publication()
-> Result<(), SourceError> {
    let mock = mock_with(
        "barbarian",
        r#"{"success":true,"sources":[{"url":"https://media.example/direct.m3u8","quality":"1080p"}]}"#,
    );
    mock.pages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            "/api/anime/search".to_string(),
            vec![(
                200,
                r#"{"results":[{"id":21,"title":"Solo Leveling"}]}"#.to_string(),
            )],
        );
    let mock = Arc::new(mock);
    let extractor = Arc::new(DelayedSub {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let provider = StreamXTV::new(
        Arc::new(TmdbClient::new("test-key", mock.clone())),
        Arc::new(ExtractorRegistry::new(vec![extractor.clone()])),
    );
    let ctx = ResolveCtx {
        fetcher: mock.as_ref(),
        media: Some(resolved()),
        source_id: None,
        referer: None,
    };
    let batches = Mutex::new(Vec::new());
    let streams = provider
        .resolve_incremental(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)), &|batch| {
            batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(batch);
        })
        .await?;
    let batches = batches.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        batches.len(),
        2,
        "each completed anime batch publishes exactly once"
    );
    assert_eq!(batches[0][0].meta.dubbed, Some(true));
    assert_eq!(batches[1][0].meta.dubbed, Some(false));
    assert_eq!(streams.len(), 3);
    assert_eq!(streams[0].url.as_str(), "https://media.example/direct.m3u8");
    assert_eq!(streams[1], batches[1][0]);
    assert_eq!(streams[2], batches[0][0]);
    assert_eq!(mock.hits("/api/stream"), ALL_PROVIDERS.len());
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| request.url.host_str() == Some("vidsrc-embed.ru"))
    );
    assert_eq!(extractor.active.load(Ordering::SeqCst), 0);
    Ok(())
}
