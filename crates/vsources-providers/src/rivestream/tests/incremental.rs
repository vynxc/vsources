use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct InFlight<'a>(&'a AtomicUsize);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct SlowApex {
    inner: Arc<ScriptedFetcher>,
    active: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
    unique_urls: bool,
}

#[async_trait]
impl Fetcher for SlowApex {
    async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _guard = InFlight(&self.active);
        if request
            .url
            .query_pairs()
            .any(|(k, v)| k == "provider" && v == "apex")
        {
            tokio::time::sleep(self.delay).await;
        }
        tokio::task::yield_now().await;
        let provider = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "provider")
            .map(|(_, v)| v.into_owned());
        let mut response = self.inner.request(request).await?;
        if self.unique_urls
            && let Some(provider) = provider
        {
            response.body = response
                .body
                .replace("movie.mp4", &format!("{provider}.mp4"));
        }
        Ok(response)
    }
}

#[tokio::test(start_paused = true)]
async fn fast_candidate_is_published_and_retained_when_slow_backend_hits_deadline()
-> Result<(), SourceError> {
    let inner = Arc::new(tmdb_mock().page("/api/provider", 200,
        r#"{"data":{"sources":[{"quality":"720p | English","url":"https://media.example/movie.mp4","source":"fast","format":"mp4"}]}}"#));
    let source = provider(&inner);
    let fetcher = SlowApex {
        inner,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        delay: Duration::from_secs(60),
        unique_urls: false,
    };
    let ctx = ResolveCtx {
        fetcher: &fetcher,
        media: None,
        source_id: None,
        referer: None,
    };
    let media = MediaRef::movie(MediaId::Tmdb(TMDB_ID));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let publish = |batch| {
        assert!(tx.send(batch).is_ok(), "receiver remains alive");
    };
    let started = tokio::time::Instant::now();
    let resolve = source.resolve_incremental(&ctx, &media, &publish);
    tokio::pin!(resolve);
    let batch = tokio::select! {
        batch = rx.recv() => batch.unwrap_or_else(|| panic!("fast backend publishes")),
        result = &mut resolve => panic!("supplier finished without early publication: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => panic!("fast backend blocked behind slow supplier"),
    };
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].url.as_str(), "https://media.example/movie.mp4");
    let final_streams = resolve.await?;
    assert_eq!(final_streams.len(), 1);
    assert_eq!(final_streams[0].url, batch[0].url);
    assert!(
        rx.try_recv().is_err(),
        "duplicate sources must not republish"
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "outer deadline cancels hung backend"
    );
    assert_eq!(
        fetcher.active.load(Ordering::SeqCst),
        0,
        "cancelled requests release resources"
    );
    assert_eq!(
        fetcher.peak.load(Ordering::SeqCst),
        2,
        "only two backend requests may run together"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn final_duplicate_winner_and_equal_quality_ties_keep_backend_priority()
-> Result<(), SourceError> {
    for unique_urls in [false, true] {
        let inner = Arc::new(tmdb_mock().page("/api/provider", 200,
            r#"{"data":{"sources":[{"quality":"720p | English","url":"https://media.example/movie.mp4","source":"fixture","format":"mp4"}]}}"#));
        let source = provider(&inner);
        let fetcher = SlowApex {
            inner,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay: Duration::from_millis(100),
            unique_urls,
        };
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: None,
        };
        let batches = Mutex::new(Vec::new());
        let streams = source
            .resolve_incremental(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)), &|batch| {
                batches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(batch);
            })
            .await?;
        let batches = batches.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            batches[0][0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("citadel")),
            "ready backend publishes first"
        );
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("apex")),
            "final winner keeps canonical priority"
        );
        if unique_urls {
            assert_eq!(streams.len(), ALL_PROVIDERS.len());
            for (stream, provider) in streams.iter().zip(ALL_PROVIDERS) {
                assert_eq!(stream.url.path(), format!("/{provider}.mp4"));
            }
        } else {
            assert_eq!(streams.len(), 1);
        }
    }
    Ok(())
}
