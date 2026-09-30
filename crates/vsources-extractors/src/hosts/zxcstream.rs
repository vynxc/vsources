//! `ZXCStream`: zxcstream embeds.
//!
//! Port of `src/extractor/ZXCStream.js` — not yet wired: returns
//! [`ExtractorError::NotFound`] until the port lands.

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

/// The `ZXCStream` extractor.
#[derive(Debug, Default)]
pub struct ZXCStream;

impl ZXCStream {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for ZXCStream {
    fn id(&self) -> &'static str {
        "zxcstream"
    }

    fn label(&self) -> &'static str {
        "ZXCStream"
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
