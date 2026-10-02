#![deny(missing_docs)]
//! Core types, traits, and utilities shared by the vsources SDK crates.
//!
//! Defines the streaming model ([`Stream`], [`StreamMeta`]), the provider and
//! HTTP abstractions ([`Source`], [`Extractor`], [`Fetcher`]), the error
//! taxonomy, TMDB identity resolution ([`TmdbClient`]), base-URL discovery
//! ([`DomainResolver`]), release-name enrichment, and the JavaScript
//! packer unpacker — everything a provider or extractor needs except the
//! concrete HTTP stack (in `vsources-net`).

pub mod audio;
pub mod domain;
pub mod enrich;
pub mod error;
pub mod ids;
pub mod language;
pub mod mappings;
pub mod resolution;
pub mod tmdb;
pub mod traits;
pub mod types;
pub mod unpack;

pub use domain::DomainResolver;
pub use error::{BlockedReason, ExtractorError, FetchError, SourceError};
pub use tmdb::{MediaName, TmdbClient};
pub use traits::{
    Extractor, FetchRequest, FetchResponse, Fetcher, ResolveCtx, ResolvedMedia, Source, fetch_head,
    fetch_json, fetch_text,
};
pub use types::{
    AudioSelection, CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream,
    StreamMeta,
};
