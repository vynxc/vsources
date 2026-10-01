//! Host extractors: one module per upstream `src/extractor/*.js` class.
//!
//! Registration order matters — it is the priority order the
//! [`ExtractorRegistry`](crate::ExtractorRegistry) walks. Keep it in sync
//! with `src/extractor/index.js`; modules document why they sit where
//! they do.

pub mod acermovies;
pub mod animedirect;
pub mod animegg;
pub mod animekai;
pub mod anipriv8;
pub mod directstream;
pub mod doodstream;
pub mod dropload;
pub mod echovideo;
pub mod embedresolver;
pub mod filemoon;
pub mod fsst;
pub mod hblinks;
pub mod hdstream4u;
pub mod hianime;
pub mod hubcloud;
pub mod hubextractor;
pub mod lulustream;
pub mod megaplay;
pub mod mixdrop;
pub mod moviebox;
pub mod netlio;
pub mod nuvio;
pub mod pantyflix;
pub mod peckle;
pub mod reanime;
pub mod savefiles;
pub mod streamembed;
pub mod supervideo;
pub mod vidara;
pub mod vidhawk;
pub mod vidking;
pub mod vidsonic;
pub mod vidsrc;
pub mod vidsrcme;
pub mod vidzee;
pub mod zxcstream;

use std::sync::Arc;

use vsources_core::traits::Extractor;

/// Every registered extractor, in upstream priority order.
///
/// Ports `createExtractors` from `src/extractor/index.js` verbatim —
/// the same order, the same comments, the same wiring (`HBLinks`
/// shares one `HubExtractor`). Extractors upstream ships but never
/// registers ([`vidsrc::VidSrc`], [`anipriv8::AniPriv8`], [`zxcstream::ZXCStream`]) stay available
/// for direct use; [`hubcloud::HubCloud`] is reached through
/// [`hubextractor::HubExtractor`]'s delegation, and upstream's terminal
/// `ExternalUrl` is the registry's `with_external_fallback` switch.
/// Nuvio is a wave-2 stub for now.
#[must_use]
pub fn all() -> Vec<Arc<dyn Extractor>> {
    let hub = Arc::new(hubextractor::HubExtractor::new());
    let hub_dyn: Arc<dyn Extractor> = hub.clone();
    vec![
        // Pantyflix — must come before Netlio so it keeps its
        // *.workers.dev URLs.
        Arc::new(pantyflix::Pantyflix::new()),
        Arc::new(animegg::AnimeGG::new()),
        Arc::new(peckle::Peckle::new()),
        Arc::new(hianime::HiAnime::new()),
        Arc::new(animekai::AnimeKai::new()),
        // Nuvio — wraps Nuvio provider streams with a Referer.
        Arc::new(nuvio::NuvioExtractor::new()),
        // Netlio — claims direct HLS before the generic fallbacks.
        Arc::new(netlio::Netlio::new()),
        Arc::new(animedirect::AnimeDirect::new()),
        Arc::new(echovideo::EchoVideo::new()),
        Arc::new(megaplay::Megaplay::new()),
        Arc::new(vidhawk::VidHawk::new()),
        Arc::new(reanime::ReAnime::new()),
        // Hub family — HBLinks shares the HubExtractor.
        hub_dyn,
        Arc::new(hblinks::HBLinks::new(hub)),
        // Direct video hosts.
        Arc::new(doodstream::DoodStream::new()),
        Arc::new(dropload::Dropload::new()),
        Arc::new(filemoon::FileMoon::new()),
        Arc::new(fsst::Fsst::new()),
        Arc::new(hdstream4u::HDStream4U::new()),
        Arc::new(lulustream::LuluStream::new()),
        Arc::new(moviebox::MovieBox::new()),
        Arc::new(savefiles::SaveFiles::new()),
        Arc::new(streamembed::StreamEmbed::new()),
        Arc::new(supervideo::SuperVideo::new()),
        Arc::new(mixdrop::MixDrop::new()),
        Arc::new(vidara::Vidara::new()),
        Arc::new(vidsonic::Vidsonic::new()),
        // VidKing — speedracelight API fallback (TMDB-based).
        Arc::new(vidking::VidKing::new()),
        // Additive passthroughs, before the fallbacks.
        Arc::new(acermovies::AcerMovies::new()),
        Arc::new(directstream::DirectStream::new()),
        // VidZee claims before EmbedResolver — its page scrape cannot
        // read the JS app.
        Arc::new(vidzee::VidZee::new()),
        // VidSrcMe claims before EmbedResolver for the same reason.
        Arc::new(vidsrcme::VidSrcMe::new()),
        // The generic fallback, last.
        Arc::new(embedresolver::EmbedResolver::new()),
    ]
}
