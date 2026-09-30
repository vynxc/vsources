//! `AniPriv8`: anipriv8 embeds.
//!
//! Port of `src/extractor/AniPriv8.js` — not yet wired: returns
//! [`ExtractorError::NotFound`] until the port lands.

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

/// The `AniPriv8` extractor.
#[derive(Debug, Default)]
pub struct AniPriv8;

impl AniPriv8 {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for AniPriv8 {
    fn id(&self) -> &'static str {
        "anipriv8"
    }

    fn label(&self) -> &'static str {
        "AniPriv8"
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
