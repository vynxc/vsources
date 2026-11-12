use super::{
    BTreeMap, CountryCode, Duration, Engine, MediaRef, ResolveCtx, Source, SourceError, SourceInfo,
    Stream, StreamExt, english_dub, enrich_stream, mpsc, ordered_snapshot, release_name_of,
    stream_key,
};
use std::collections::HashSet;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct AttemptPolicy {
    pub timeout: Duration,
    pub strict_english: bool,
    pub language: Option<CountryCode>,
}

impl Engine {
    pub(super) async fn drain_incremental(
        &self,
        source: &dyn Source,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        policy: AttemptPolicy,
        publish: &(dyn Fn(Vec<Stream>) + Send + Sync),
    ) -> Result<Vec<Stream>, SourceError> {
        let limit = self.stream_limit;
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
        let raw_seen = Mutex::new(HashSet::new());
        let publish_raw = |batch: Vec<Stream>| {
            let mut seen = raw_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ready: Vec<_> = batch
                .into_iter()
                .filter(|stream| !policy.strict_english || english_dub(stream))
                .filter(|stream| seen.len() < limit && seen.insert(stream_key(stream)))
                .collect();
            if !ready.is_empty() {
                let _ = raw_tx.send(ready);
            }
        };
        let mut resolving = Some(Box::pin(tokio::time::timeout(policy.timeout, async {
            if source.supports_english_audio_selection()
                && (policy.strict_english || policy.language == Some(CountryCode::En))
            {
                source.resolve_english_dub(ctx, media).await
            } else {
                source.resolve_incremental(ctx, media, &publish_raw).await
            }
        })));
        let authoritative = AtomicBool::new(false);
        let mut failure = None;
        let mut partial = Vec::new();
        let mut partial_keys = HashSet::new();
        let mut validating: futures::stream::FuturesUnordered<
            futures::future::BoxFuture<'_, (bool, Vec<Stream>)>,
        > = futures::stream::FuturesUnordered::new();
        loop {
            tokio::select! {
                outcome = async { resolving.as_mut().unwrap_or_else(|| unreachable!()).await }, if resolving.is_some() => {
                    match outcome {
                        Ok(Ok(streams)) => {
                            authoritative.store(true, Ordering::SeqCst);
                            let streams = prepare(source.info(), streams, limit, policy.strict_english);
                            let language = policy.language;
                            let strict_english = policy.strict_english;
                            validating.push(Box::pin(async move {
                                (true, self.validate_batch(streams, language, strict_english, publish).await)
                            }));
                        }
                        Ok(Err(error)) => { failure = Some(error); }
                        Err(_) => { failure = Some(SourceError::scrape(&source.info().id, "resolve timed out")); }
                    }
                    // Drop the owning timeout future before draining validation to cancel upstream work.
                    resolving = None;
                }
                Some(batch) = raw_rx.recv() => {
                    if !authoritative.load(Ordering::SeqCst) {
                        let streams = prepare(source.info(), batch, limit, policy.strict_english);
                        let authoritative = &authoritative;
                        let language = policy.language;
                        let strict_english = policy.strict_english;
                        validating.push(Box::pin(async move {
                            let streams = self.validate_batch(streams, language, strict_english, &|batch| {
                                if !authoritative.load(Ordering::SeqCst) { publish(batch); }
                            }).await;
                            (false, streams)
                        }));
                    }
                }
                Some((is_final, streams)) = validating.next(), if !validating.is_empty() => {
                    if is_final { return Ok(streams); }
                    for stream in streams {
                        if partial.len() < limit && partial_keys.insert(stream_key(&stream)) { partial.push(stream); }
                    }
                }
            }
            if resolving.is_none() && raw_rx.is_empty() && validating.is_empty() {
                break;
            }
        }
        if !partial.is_empty() {
            return Ok(partial);
        }
        Err(failure.unwrap_or(SourceError::NotFound))
    }
    async fn validate_batch(
        &self,
        streams: Vec<Stream>,
        language: Option<CountryCode>,
        strict_english: bool,
        publish: &(dyn Fn(Vec<Stream>) + Send + Sync),
    ) -> Vec<Stream> {
        let streams = self
            .probes
            .filter_progressive(self.fetcher.as_ref(), streams, language, |batch| {
                let batch = qualified(batch, strict_english);
                if !batch.is_empty() {
                    publish(batch);
                }
            })
            .await;
        qualified(streams, strict_english)
    }
}

fn prepare(
    info: &SourceInfo,
    mut streams: Vec<Stream>,
    limit: usize,
    strict_english: bool,
) -> Vec<Stream> {
    if strict_english {
        streams.retain(english_dub);
    }
    streams.truncate(limit);
    for stream in &mut streams {
        stream.meta.source_id = Some(info.id.clone());
        stream.meta.source_label = Some(info.label.clone());
        let release_name = release_name_of(&stream.url);
        enrich_stream(stream, &release_name);
    }
    let mut seen = HashSet::new();
    streams.retain(|stream| seen.insert(stream_key(stream)));
    streams
}

fn qualified(mut streams: Vec<Stream>, strict_english: bool) -> Vec<Stream> {
    if strict_english {
        streams.retain(english_dub);
    }
    streams
}

#[derive(Default)]
pub(super) struct ProviderViews {
    views: BTreeMap<usize, Vec<Stream>>,
    completed: HashSet<usize>,
}

impl ProviderViews {
    pub fn append(&mut self, index: usize, batch: Vec<Stream>, limit: usize) {
        if self.completed.contains(&index) {
            return;
        }
        let view = self.views.entry(index).or_default();
        for stream in batch {
            let key = stream_key(&stream);
            if let Some(existing) = view.iter_mut().find(|existing| stream_key(existing) == key) {
                *existing = stream;
            } else if view.len() < limit {
                view.push(stream);
            }
        }
    }

    pub fn finish(&mut self, index: usize, streams: Vec<Stream>) {
        self.completed.insert(index);
        self.views.insert(index, streams);
    }

    pub fn snapshot(&self, limit: usize) -> Vec<Stream> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for (&index, streams) in &self.views {
            for (sequence, stream) in streams.iter().enumerate() {
                if entries.len() < limit && seen.insert(stream_key(stream)) {
                    entries.push((
                        index,
                        u32::try_from(sequence).unwrap_or(u32::MAX),
                        stream.clone(),
                    ));
                }
            }
        }
        ordered_snapshot(&entries)
    }
}
