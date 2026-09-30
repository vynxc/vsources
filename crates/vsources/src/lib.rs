#![deny(missing_docs)]
//! The vsources streaming SDK.
//!
//! Resolve free streaming sources for movies and series from a single
//! embeddable library — no HTTP server required.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use vsources::{EngineBuilder, MediaId, MediaRef, MediaType};
//!
//! let engine = EngineBuilder::new()
//!     // .flaresolverr(url::Url::parse("http://localhost:8191/")?)
//!     .build()?;
//! let media = MediaRef {
//!     id: MediaId::tmdb(272_05),
//!     kind: MediaType::Movie,
//!     season: None,
//!     episode: None,
//! };
//! let streams = engine.resolve(&media).await?;
//! # let _ = streams; Ok(())
//! # }
//! ```

pub use engine::{Engine, EngineBuilder, EngineError};
pub use vsources_core::traits::{Fetcher, ResolveCtx, Source};
pub use vsources_core::types::{MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta};
pub use vsources_providers::{CachedSource, SourceRegistry};

pub use vsources_core::{error, ids, traits, types};

pub mod engine;
pub mod liveness;
