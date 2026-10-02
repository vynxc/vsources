//! Shared adapter helpers. Site catalog keys are never treated as database ids.

use vsources_core::mappings::{MappingService, SeasonIds};
use vsources_core::traits::ResolveCtx;
use vsources_core::types::MediaId;
use vsources_core::types::MediaRef;

/// Resolve the requested season, trying both external identities when available.
pub(crate) async fn ids(
    mappings: Option<&MappingService>,
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
) -> Option<SeasonIds> {
    let mappings = mappings?;
    let meta = ctx.media.as_ref();
    let title = meta.map(|meta| meta.name.as_str());
    let season = media.season.unwrap_or(1);
    let imdb = meta
        .and_then(|meta| meta.imdb_id.as_deref())
        .or(match &media.id {
            MediaId::Imdb(id) => Some(id.as_str()),
            MediaId::Tmdb(_) => None,
        });
    if let Some(imdb) = imdb
        && let Ok(Some(ids)) = mappings.season_ids_by_imdb(imdb, season, title).await
    {
        return Some(ids);
    }
    let tmdb = meta.and_then(|meta| meta.tmdb_id).or(match media.id {
        MediaId::Tmdb(id) => Some(id),
        MediaId::Imdb(_) => None,
    });
    mappings
        .season_ids_by_tmdb(tmdb?, season, title)
        .await
        .ok()
        .flatten()
}

/// A season's canonical database title for sites that expose only title search.
/// This remains a title candidate, not a verified site identity.
pub(crate) async fn title_context<'a>(
    mappings: Option<&MappingService>,
    ctx: &ResolveCtx<'a>,
    media: &MediaRef,
) -> Option<ResolveCtx<'a>> {
    let ids = ids(mappings, ctx, media).await?;
    let entry = mappings?
        .anilist_by_id(ids.anilist_id)
        .await
        .ok()
        .flatten()?;
    let name = entry.english.or(entry.romaji)?;
    let mut meta = ctx.media.clone()?;
    if name == meta.name {
        return None;
    }
    meta.name = name;
    // A show's premiere year is not the premiere year of a later anime cour.
    if media.season.is_some_and(|season| season > 1) {
        meta.year = None;
    }
    Some(ResolveCtx {
        fetcher: ctx.fetcher,
        media: Some(meta),
        source_id: ctx.source_id,
        referer: ctx.referer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedFetcher;
    use std::sync::Arc;
    use vsources_core::traits::{Fetcher, ResolvedMedia};

    #[tokio::test]
    async fn title_only_sites_get_the_requested_season_title_and_keep_episode_context() {
        let fetcher = Arc::new(
            ScriptedFetcher::default()
                .page(
                    |url| url.host_str() == Some("arm.haglund.dev"),
                    r#"[
                {"anilist":11,"themoviedb-season":1},
                {"anilist":22,"themoviedb-season":2}
            ]"#,
                )
                .page(
                    |url| url.host_str() == Some("graphql.anilist.co"),
                    r#"{"data":{"Media":{"id":22,"title":{"english":"Show Season 2"}}}}"#,
                ),
        );
        let mappings = MappingService::new(fetcher.clone());
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref() as &dyn Fetcher,
            media: Some(ResolvedMedia {
                tmdb_id: Some(123),
                imdb_id: None,
                name: "Show".into(),
                year: Some(2020),
                season: Some(2),
                episode: Some(3),
            }),
            source_id: Some("site"),
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(123), 2, 3);
        let mapped = title_context(Some(&mappings), &ctx, &media)
            .await
            .unwrap_or_else(|| panic!("mapped title"));
        let meta = mapped.media.unwrap_or_else(|| panic!("metadata"));
        assert_eq!(meta.name, "Show Season 2");
        assert_eq!(meta.year, None);
        assert_eq!(meta.episode, Some(3));
        assert_eq!(meta.tmdb_id, Some(123));
        assert_eq!(mapped.source_id, Some("site"));
        assert_eq!(
            ctx.media.as_ref().map(|meta| meta.name.as_str()),
            Some("Show")
        );
    }
}
