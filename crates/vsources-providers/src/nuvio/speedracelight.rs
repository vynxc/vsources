//! Shared speedracelight client, available at the original provider path.
//!
//! The implementation lives below providers so the `VidKing` extractor and
//! Nuvio providers use the same protocol and seed-cache types.
pub use vsources_extractors::speedracelight::*;
