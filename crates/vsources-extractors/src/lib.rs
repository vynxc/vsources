#![deny(missing_docs)]
//! Extractors: resolve embed/player URLs into direct playable streams.
//!
//! Ports the upstream `src/extractor/` family: every host ships one
//! extractor module, and the [`ExtractorRegistry`] routes a URL through
//! them with a fallback chain, a result cache, and in-flight coalescing.
//!
//! Host gate hotlinked media on `Referer`; where the upstream stack
//! rebuilt a server-side proxy URL, streams here carry the required
//! headers in [`StreamMeta::request_headers`](vsources_core::types::StreamMeta) so any player (Tauri,
//! Android, server) can apply them directly.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use vsources_core::traits::{Fetcher, ResolveCtx};
//! use vsources_extractors::hosts;
//! use vsources_extractors::ExtractorRegistry;
//!
//! let fetcher = vsources_net::ChromeFetcher::builder().build()?;
//! let registry = ExtractorRegistry::new(hosts::all());
//! let ctx = ResolveCtx { fetcher: &fetcher, media: None, source_id: None, referer: None };
//! let streams = registry
//!     .extract(&ctx, &url::Url::parse("https://filemoon.to/e/abc")?)
//!     .await?;
//! # let _ = streams; Ok(())
//! # }
//! ```

pub mod helpers;
pub mod hosts;
pub mod registry;
pub mod speedracelight;

#[cfg(test)]
pub(crate) mod testing;

pub use hosts::doodstream::DoodStream;
pub use hosts::filemoon::FileMoon;
pub use registry::ExtractorRegistry;
