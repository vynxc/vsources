//! Nuvio: wraps Nuvio provider streams with a Referer when needed.
//!
//! Port of `src/extractor/NuvioExtractor.js` — not yet wired: returns
//! [`ExtractorError::NotFound`] until the port lands.

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

/// The Nuvio extractor.
#[derive(Debug, Default)]
pub struct NuvioExtractor;

impl NuvioExtractor {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for NuvioExtractor {
    fn id(&self) -> &'static str {
        "nuvio"
    }

    fn label(&self) -> &'static str {
        "Nuvio"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        // TODO(port): the upstream host match.
        let _ = url;
        false
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let _ = url;
        Err(ExtractorError::NotFound)
    }
}
