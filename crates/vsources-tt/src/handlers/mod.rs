//! The default handler table, ported section by section from the
//! TypeScript `handlers.ts` in the original order.

mod audio_final;
mod complete;
mod date;
mod dubbed2;
mod edition;
mod episode_code;
mod episodes;
mod extension;
mod group;
mod languages_basic;
mod languages_detailed;
mod misc_pt_es;
mod network;
mod ppv;
mod quality;
mod release;
mod resolution;
mod seasons;
mod site;
mod site2;
mod subbed2;
mod subbed_dubbed;
mod technical;
mod title;
mod volumes;
mod year;

use std::sync::OnceLock;

use crate::types::Handler;

/// All default handlers in the exact original order.
pub fn all() -> &'static [Handler] {
    static HANDLERS: OnceLock<Vec<Handler>> = OnceLock::new();
    HANDLERS.get_or_init(|| {
        let mut all = Vec::new();
        all.extend(title::handlers());
        all.extend(ppv::handlers());
        all.extend(site::handlers());
        all.extend(episode_code::handlers());
        all.extend(resolution::handlers());
        all.extend(date::handlers());
        all.extend(year::handlers());
        all.extend(edition::handlers());
        all.extend(release::handlers());
        all.extend(quality::handlers());
        all.extend(technical::handlers());
        all.extend(volumes::handlers());
        all.extend(languages_basic::handlers());
        all.extend(complete::handlers());
        all.extend(seasons::handlers());
        all.extend(episodes::handlers());
        all.extend(subbed_dubbed::handlers());
        all.extend(languages_detailed::handlers());
        all.extend(misc_pt_es::handlers());
        all.extend(subbed2::handlers());
        all.extend(dubbed2::handlers());
        all.extend(site2::handlers());
        all.extend(network::handlers());
        all.extend(group::handlers());
        all.extend(extension::handlers());
        all.extend(audio_final::handlers());
        all
    })
}
