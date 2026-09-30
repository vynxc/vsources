//! `VidSrcMe`: vidsrc.me/vidsrcme.ru chain — data API + decrypt to HLS.
//!
//! Port of `src/extractor/VidSrcMe.js` — not yet wired: returns
//! [`ExtractorError::NotFound`] until the port lands.

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

/// The `VidSrcMe` extractor.
#[derive(Debug, Default)]
pub struct VidSrcMe;

impl VidSrcMe {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for VidSrcMe {
    fn id(&self) -> &'static str {
        "vidsrcme"
    }

    fn label(&self) -> &'static str {
        "VidSrcMe"
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
