//! Id-based provider mappings: anime database lookups shared by all providers.
//!
//! The upstream providers resolved catalog entries by scoring titles — the
//! exact fragile step that breaks on season splits (`A Sword` S1/S2 share one
//! `IMDb` id, so a name search lands on whichever entry scored higher). This
//! module replaces that with id equality wherever the ids are available:
//!
//! - [`ArmClient`] — `arm.haglund.dev`, the `AODB` ∪ `Anime-Lists` merge the
//!   production Stremio anime addons resolve through. One `IMDb`/`TMDB` id
//!   maps to one *array* of per-season entries (`{anilist, myanimelist,
//!   themoviedb-season, …}`), refreshed upstream every 24 h.
//! - [`AniListClient`] — the public `graphql.anilist.co` API (no auth): id
//!   lookups and title search, the fallback when `arm` lacks the show.
//! - [`MappingService`] — the cheap-clone façade providers resolve through:
//!   one shared fetch per lookup key (in-flight dedup, like
//!   [`TmdbClient`](crate::tmdb::TmdbClient) does for the ~70-provider
//!   fan-out), TTL caches, and a 429-aware retry.

pub mod anilist;
pub mod arm;
pub mod service;

pub use anilist::AniListClient;
pub use arm::ArmClient;
pub use service::{MappingService, SeasonIds};
