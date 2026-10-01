#![deny(missing_docs)]
//! Streaming providers: search sites and resolve `MediaRef`s to streams.
//!
//! Every provider is one module implementing [`Source`]; the
//! [`SourceRegistry`] wires them together with per-provider result
//! caching, negative caching, and priority ordering. Not-found answers
//! surface as empty results, so fan-out callers only ever see errors
//! worth failing on.
//!
//! The catalog is English-only by policy: the port keeps the
//! English-primary and anime (English-subbed) subset of the upstream
//! provider list and drops everything else. Adding a provider back is
//! one module plus one registry entry.
//!
//! Wave 1 — the 25 self-contained scrapers — is assembled by
//! [`wave1`]; the Nuvio-backed waves 2a/2b follow.
//!
//! [`Source`]: vsources_core::traits::Source

pub mod acermovies;
pub mod allwish;
pub mod anibd;
pub mod anichan;
pub mod anidoor;
pub mod anikage;
pub mod anikoto;
pub mod anikototv;
pub mod animeflix;
pub mod animegg;
pub mod animekai;
pub mod animesuge;
pub mod animezey;
pub mod animotvslash;
pub mod aniwaves;
pub mod atlantic;
pub mod cache;
pub mod cineby;
pub mod cinebyrocks;
pub mod cinejoyallinone;
pub mod cinewave;
pub mod framex;
pub mod hianime;
pub mod imdbplay;
pub mod itachi;
pub mod moviebox;
pub mod necro;
pub mod netlio;
pub mod nkastream;
pub mod nowhdtime;
pub mod nuvio;
pub mod peckle;
pub mod playimdb;
pub mod primeshows;
pub mod raflix;
pub mod reanime;
pub mod registry;
pub mod rivestream;
pub mod stellar;
pub mod streamxtv;
pub mod twodhive;
pub mod videasy;
pub mod videasyto;
pub mod vidfast;
pub mod vidking;
pub mod vidlink;
pub mod vidsrcsbs;
pub mod vidzee;
pub mod vixsrc;
pub mod watchseries;
pub mod zxcstream;

#[cfg(test)]
pub(crate) mod testing;

use std::sync::Arc;

use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::Source;
use vsources_extractors::ExtractorRegistry;

pub use cache::CachedSource;
pub use registry::SourceRegistry;

/// The wave-1 provider set: the 25 self-contained English scrapers.
///
/// One TMDB client is shared by every provider that needs id or title
/// metadata, and one extractor registry serves every embed resolution.
/// Peckle's `FebBox` cookie is optional: set `PECKLE_FEBBOX_COOKIE` to
/// ride an authenticated session, else it resolves anonymously.
///
/// Waves 2a/2b (the Nuvio-backed providers) are not part of this set
/// yet.
#[must_use]
pub fn wave1(tmdb: Arc<TmdbClient>) -> Vec<Arc<dyn Source>> {
    let extractors = Arc::new(ExtractorRegistry::new(vsources_extractors::hosts::all()));
    let febbox_cookie = std::env::var("PECKLE_FEBBOX_COOKIE").ok();

    vec![
        // Anime (12).
        Arc::new(aniwaves::AniWaves::new()),
        Arc::new(allwish::AllWish::new(Arc::clone(&extractors))),
        Arc::new(anibd::AniBD::new()),
        Arc::new(anidoor::AniDoor::new(Arc::clone(&extractors))),
        Arc::new(anikage::AniKage::new()),
        Arc::new(anikoto::Anikoto::new(Arc::clone(&extractors))),
        Arc::new(animeflix::AnimeFlix::new(Arc::clone(&extractors))),
        Arc::new(animegg::AnimeGG::new(Arc::clone(&extractors))),
        Arc::new(animekai::AnimeKai::new(Arc::clone(&extractors))),
        Arc::new(hianime::HiAnime::new(Arc::clone(&extractors))),
        Arc::new(itachi::Itachi::new(Arc::clone(&extractors))),
        Arc::new(twodhive::TwoDhive::new(Arc::clone(&extractors))),
        // Movies and TV (13).
        Arc::new(cinewave::CineWave::new(
            Arc::clone(&tmdb),
            Arc::clone(&extractors),
        )),
        Arc::new(imdbplay::IMDBPlay::new(Arc::clone(&tmdb))),
        Arc::new(moviebox::MovieBox::new(Arc::clone(&tmdb)).with_environment_mobile_key()),
        Arc::new(necro::Necro::new(
            Arc::clone(&tmdb),
            Arc::clone(&extractors),
        )),
        Arc::new(netlio::Netlio::new(Arc::clone(&tmdb))),
        Arc::new(nowhdtime::NowHDTime::new(Arc::clone(&tmdb))),
        Arc::new(peckle::Peckle::new(febbox_cookie, Arc::clone(&tmdb))),
        Arc::new(primeshows::PrimeShows::new(
            Arc::clone(&extractors),
            Arc::clone(&tmdb),
        )),
        Arc::new(vidfast::VidFast::new(
            Arc::clone(&extractors),
            Arc::clone(&tmdb),
        )),
        Arc::new(vidking::VidKing::new(
            Arc::clone(&extractors),
            Arc::clone(&tmdb),
        )),
        Arc::new(vidsrcsbs::VidSrcSbs::new(
            Arc::clone(&extractors),
            Arc::clone(&tmdb),
        )),
        Arc::new(vidzee::VidZee::new(
            Arc::clone(&extractors),
            Arc::clone(&tmdb),
        )),
        Arc::new(watchseries::WatchSeries::new(Arc::clone(&extractors), tmdb)),
    ]
}
/// The wave-2 provider set: the 23 Nuvio-backed providers (8 anime,
/// 15 movies/TV).
///
/// Like [`wave1`], one TMDB client is shared; one extractor registry
/// serves the embed-resolving providers, and one speedracelight
/// [`SeedStore`](nuvio::speedracelight::SeedStore) is shared by the
/// three VidKing-family consumers (the upstream `srlSeed` singleton).
#[must_use]
pub fn wave2(tmdb: Arc<TmdbClient>) -> Vec<Arc<dyn Source>> {
    let extractors = Arc::new(ExtractorRegistry::new(vsources_extractors::hosts::all()));
    let seeds = Arc::new(nuvio::speedracelight::SeedStore::new());

    vec![
        // Anime (8).
        Arc::new(anichan::AniChan::new(Arc::clone(&tmdb))),
        Arc::new(anikototv::AnikotoTV::new(Arc::clone(&tmdb))),
        Arc::new(animesuge::AnimeSuge::new(Arc::clone(&tmdb))),
        Arc::new(animezey::AnimeZeY::new(Arc::clone(&tmdb))),
        Arc::new(animotvslash::AniMoTVSlash::new(Arc::clone(&tmdb))),
        Arc::new(nkastream::NikaStream::new(Arc::clone(&tmdb))),
        Arc::new(reanime::ReAnime::new(Arc::clone(&tmdb))),
        Arc::new(streamxtv::StreamXTV::new(
            Arc::clone(&tmdb),
            Arc::clone(&extractors),
        )),
        // Movies and TV (15).
        Arc::new(acermovies::AcerMovies::new(Arc::clone(&tmdb))),
        Arc::new(atlantic::Atlantic::new(Arc::clone(&tmdb))),
        Arc::new(cineby::Cineby::new(Arc::clone(&tmdb), Arc::clone(&seeds))),
        Arc::new(cinebyrocks::CinebyRocks::new(Arc::clone(&tmdb))),
        Arc::new(cinejoyallinone::CineJoyAllInOne::new(Arc::clone(&tmdb))),
        Arc::new(framex::FrameX::new(Arc::clone(&tmdb))),
        Arc::new(playimdb::PlayImdb::new(Arc::clone(&tmdb))),
        Arc::new(raflix::Raflix::new(
            Arc::clone(&tmdb),
            Arc::clone(&extractors),
        )),
        Arc::new(rivestream::RiveStream::new(Arc::clone(&tmdb))),
        Arc::new(stellar::Stellar::new(Arc::clone(&tmdb))),
        Arc::new(videasy::VidEasy::new(Arc::clone(&tmdb), Arc::clone(&seeds))),
        Arc::new(videasyto::VideasyTo::new(
            Arc::clone(&tmdb),
            Arc::clone(&seeds),
        )),
        Arc::new(vidlink::VidLink::new(Arc::clone(&tmdb))),
        Arc::new(vixsrc::VixSrc::new(Arc::clone(&tmdb))),
        Arc::new(zxcstream::ZXCStream::new(tmdb)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TMDB client over a fetcher that is never called: `wave1` only
    /// wires constructors, and none of them performs I/O.
    fn tmdb() -> Arc<TmdbClient> {
        Arc::new(TmdbClient::new(
            "test-key",
            Arc::new(crate::testing::NoopFetcher),
        ))
    }

    #[test]
    fn wave1_registers_all_25_providers() {
        let sources = wave1(tmdb());
        assert_eq!(sources.len(), 25);
    }

    #[test]
    fn wave2_registers_all_23_providers() {
        let sources = wave2(tmdb());
        assert_eq!(sources.len(), 23);
    }

    #[test]
    fn wave2_ids_are_unique_and_disjoint_from_wave1() {
        let wave2_ids: Vec<String> = wave2(tmdb()).iter().map(|s| s.info().id.clone()).collect();
        let count = wave2_ids.len();
        let mut sorted = wave2_ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "duplicate provider ids in wave2");
        let wave1_ids: Vec<String> = wave1(tmdb()).iter().map(|s| s.info().id.clone()).collect();
        let overlap = wave2_ids.iter().filter(|id| wave1_ids.contains(id)).count();
        assert_eq!(overlap, 0, "wave1/wave2 id collision");
    }

    #[test]
    fn wave1_ids_are_unique() {
        let sources = wave1(tmdb());
        let mut ids: Vec<&str> = sources.iter().map(|s| s.info().id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate provider ids in wave1");
    }

    #[test]
    fn wave1_priorities_are_set() {
        // Upstream assigns non-default priorities to several wave-1
        // providers; zero-everywhere would mean the field was dropped
        // during the port.
        let sources = wave1(tmdb());
        let nonzero = sources.iter().filter(|s| s.info().priority != 0).count();
        assert!(nonzero > 0, "no wave-1 provider carries a priority");
    }
}
