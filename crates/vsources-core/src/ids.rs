//! Media id parsing and formatting.
//!
//! Ports `src/utils/id.js` on top of the typed [`MediaRef`] model.

use crate::error::SourceError;
use crate::types::{MediaId, MediaRef};

/// Parse a `tt1234567`, `tmdb:1396`, or bare `1396` id with optional
/// `:season:episode` parts into a [`MediaRef`].
pub fn parse_media_ref(
    input: &str,
    kind: crate::types::MediaType,
) -> Result<MediaRef, SourceError> {
    let parts: Vec<&str> = input.split(':').collect();
    let Some(head) = parts.first() else {
        return Err(SourceError::scrape(
            "id",
            format!("media id {input:?} is invalid"),
        ));
    };
    // The optional `tmdb:` prefix shifts the id/season/episode parts
    // by one; bare and `tt` forms keep the upstream layout.
    let offset = usize::from(head.eq_ignore_ascii_case("tmdb"));
    let id = if head.starts_with("tt") && head.len() > 2 {
        MediaId::Imdb(head.to_string())
    } else if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
        MediaId::Tmdb(
            head.parse::<u64>()
                .map_err(|_| SourceError::scrape("id", format!("media id {input:?} is invalid")))?,
        )
    } else if offset == 1 {
        let digits = parts.get(1).copied().unwrap_or_default();
        let id = digits
            .parse::<u64>()
            .map_err(|_| SourceError::scrape("id", format!("media id {input:?} is invalid")))?;
        MediaId::Tmdb(id)
    } else {
        return Err(SourceError::scrape(
            "id",
            format!("media id {input:?} is invalid"),
        ));
    };
    let season = parts.get(1 + offset).and_then(|s| s.parse::<u32>().ok());
    let episode = parts.get(2 + offset).and_then(|s| s.parse::<u32>().ok());
    Ok(MediaRef {
        id,
        kind,
        season,
        episode,
    })
}

/// `S01E05` formatting for a media reference with season/episode.
#[must_use]
pub fn format_season_and_episode(media: &MediaRef) -> String {
    media.format_season_and_episode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MediaType;

    #[test]
    fn parses_every_documented_media_id_form() {
        use crate::types::MediaId;

        // The `tmdb:` prefix form the CLI documents.
        assert_eq!(MediaId::parse("tmdb:27205"), Some(MediaId::Tmdb(27_205)));
        assert_eq!(MediaId::parse("TMDB:27205"), Some(MediaId::Tmdb(27_205)));
        // Bare digits and `tt` forms (the upstream id.js layout).
        assert_eq!(MediaId::parse("27205"), Some(MediaId::Tmdb(27_205)));
        assert_eq!(
            MediaId::parse("tt1375666"),
            Some(MediaId::Imdb("tt1375666".to_string()))
        );
        // Rejections.
        assert_eq!(MediaId::parse("tmdb:"), None);
        assert_eq!(MediaId::parse("tmdb:abc"), None);
        assert_eq!(MediaId::parse(""), None);
        assert_eq!(MediaId::parse("tmdb:27205:1:2"), None);
    }

    #[test]
    fn parses_media_refs_with_season_and_episode() {
        // Bare form: id:season:episode — the upstream layout.
        let media = parse_media_ref("1396:1:2", MediaType::Series)
            .unwrap_or_else(|e| panic!("valid ref: {e}"));
        assert_eq!(media.id, MediaId::Tmdb(1396));
        assert_eq!(media.season, Some(1));
        assert_eq!(media.episode, Some(2));

        // `tmdb:` form shifts the parts by one.
        let media = parse_media_ref("tmdb:1396:1:2", MediaType::Series)
            .unwrap_or_else(|e| panic!("valid ref: {e}"));
        assert_eq!(media.id, MediaId::Tmdb(1396));
        assert_eq!(media.season, Some(1));
        assert_eq!(media.episode, Some(2));

        // IMDb form keeps the upstream layout.
        let media = parse_media_ref("tt0944947:1:2", MediaType::Series)
            .unwrap_or_else(|e| panic!("valid ref: {e}"));
        assert_eq!(media.id, MediaId::Imdb("tt0944947".to_string()));
        assert_eq!(media.season, Some(1));
        assert_eq!(media.episode, Some(2));

        assert!(parse_media_ref("tmdb:", MediaType::Movie).is_err());
        assert!(parse_media_ref("!!", MediaType::Movie).is_err());
    }
}
