//! The priority-ordered provider registry.

use std::sync::Arc;

use vsources_core::traits::Source;
use vsources_core::types::SourceInfo;

use crate::cache::CachedSource;

/// The registered providers, cached and priority-ordered.
///
/// Construction wraps every source in [`CachedSource`] and sorts by
/// descending `priority` (ties broken by id), so iteration order is the
/// upstream resolution order and repeated lookups are served from the
/// per-provider caches with the upstream "NotFound-to-empty" semantics.
pub struct SourceRegistry {
    /// The wrapped providers, in resolution order.
    sources: Vec<Arc<CachedSource>>,
}

impl SourceRegistry {
    /// Register `sources` with caching and priority ordering.
    #[must_use]
    pub fn new(sources: Vec<Arc<dyn Source>>) -> Self {
        let mut sources: Vec<Arc<CachedSource>> = sources
            .into_iter()
            .map(|source| Arc::new(CachedSource::new(source)))
            .collect();
        sources.sort_by(|a, b| {
            let (a, b) = (a.info(), b.info());
            b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id))
        });
        Self { sources }
    }

    /// All registered sources in priority order.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<dyn Source>> {
        self.sources
            .iter()
            .map(|source| {
                let dyn_source: Arc<dyn Source> = source.clone();
                dyn_source
            })
            .collect()
    }

    /// The source with this id, when registered.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn Source>> {
        let found = self.sources.iter().find(|source| source.info().id == id)?;
        let dyn_source: Arc<dyn Source> = found.clone();
        Some(dyn_source)
    }

    /// The registered sources' descriptors, in priority order.
    #[must_use]
    pub fn list(&self) -> Vec<&SourceInfo> {
        self.sources.iter().map(|source| source.info()).collect()
    }

    /// How many providers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Whether no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    use vsources_core::error::SourceError;

    use crate::testing::{CountingSource, Outcome, stub_ctx, stub_media};

    fn counting(id: &str, priority: i32, outcome: Outcome) -> Arc<CountingSource> {
        Arc::new(CountingSource {
            info: crate::testing::stub_info(id, priority),
            calls: std::sync::atomic::AtomicUsize::new(0),
            outcome,
        })
    }

    #[tokio::test]
    async fn orders_by_priority_then_id() {
        let low = counting("low", 1, Outcome::Empty);
        let high = counting("high", 5, Outcome::Empty);
        let tie = counting("aaa", 5, Outcome::Empty);
        let sources: Vec<Arc<dyn Source>> = vec![low.clone(), high.clone(), tie.clone()];
        let registry = SourceRegistry::new(sources);

        let order: Vec<String> = registry
            .list()
            .into_iter()
            .map(|info| info.id.clone())
            .collect();
        assert_eq!(order, vec!["aaa", "high", "low"]);
    }

    #[tokio::test]
    async fn get_finds_by_id() {
        let source = counting("stub", 0, Outcome::OneStream);
        let sources: Vec<Arc<dyn Source>> = vec![source.clone()];
        let registry = SourceRegistry::new(sources);

        let Some(found) = registry.get("stub") else {
            panic!("registered source missing");
        };
        assert_eq!(found.info().id, "stub");
        assert!(registry.get("missing").is_none());
    }

    #[tokio::test]
    async fn registry_resolution_is_cached_and_empty_on_not_found() -> Result<(), SourceError> {
        let source = counting("stub", 0, Outcome::NotFound);
        let sources: Vec<Arc<dyn Source>> = vec![source.clone()];
        let registry = SourceRegistry::new(sources);
        let ctx = stub_ctx();
        let media = stub_media();

        let Some(found) = registry.get("stub") else {
            panic!("registered source missing");
        };
        let first = found.resolve(&ctx, &media).await?;
        let second = registry
            .all()
            .first()
            .ok_or(SourceError::scrape("registry", "no sources registered"))?
            .resolve(&ctx, &media)
            .await?;
        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn len_and_empty() {
        let registry = SourceRegistry::new(Vec::new());
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        let sources: Vec<Arc<dyn Source>> = vec![counting("stub", 0, Outcome::Empty)];
        let registry = SourceRegistry::new(sources);
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }
}
