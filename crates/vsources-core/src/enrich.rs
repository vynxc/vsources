//! Release-name enrichment for streams.
//!
//! Folds the structured metadata produced by
//! [`vsources_tt::parse_torrent_title`] into a [`StreamMeta`]: resolution,
//! quality, codec, audio, languages, and size. Providers that scrape
//! torrent-style file names (the Nuvio/mirror families) get full stream
//! metadata without hand-rolled parsing.
//!
//! This is vsources-native glue (the upstream has no equivalent); it only
//! fills fields the stream has not set, so a provider's own, more precise
//! metadata always wins.

use crate::language::find_country_codes;
use crate::resolution::find_height;
use crate::types::{CountryCode, Stream, StreamMeta};

/// Fold release-name metadata into `meta`, keeping already-set fields.
///
/// Parses `release_name` with the shared torrent-title parser and copies
/// the resulting quality, resolution, codec, audio, languages, and size
/// into any field `meta` has not populated yet.
pub fn enrich_stream_meta(meta: &mut StreamMeta, release_name: &str) {
    let parsed = vsources_tt::parse_torrent_title(release_name);

    if meta.quality.is_none() {
        meta.quality = parsed.quality;
    }
    if meta.resolution.is_none() {
        meta.resolution = parsed.resolution.as_deref().and_then(find_height);
    }
    if meta.codec.is_none() {
        meta.codec = parsed.codec;
    }
    if meta.audio.is_empty()
        && let Some(audio) = parsed.audio
    {
        meta.audio = audio;
    }
    if meta.languages.is_empty() {
        // PTT language tags are a superset of stream country codes;
        // everything unrecognized is dropped rather than guessed.
        meta.languages = parsed
            .languages
            .unwrap_or_default()
            .iter()
            .flat_map(|tag| tag.split_whitespace())
            .filter_map(CountryCode::parse)
            .collect::<Vec<_>>();
        if meta.languages.is_empty() {
            // Fall back to scanning the whole release name for tags —
            // many names say "Dual Audio" or "ENG" instead of tagging.
            meta.languages = find_country_codes(release_name);
        }
    }
    if meta.size.is_none() && meta.size_label.is_none() {
        meta.size_label = parsed.size;
    }
    if meta.dubbed.is_none() {
        meta.dubbed = parsed.dubbed;
    }
    if meta.subbed.is_none() {
        meta.subbed = parsed.subbed;
    }
}
/// Fold release-name metadata into a stream's metadata.
///
/// Convenience wrapper over [`enrich_stream_meta`] for the common
/// "parse the file name into the stream" case.
pub fn enrich_stream(stream: &mut Stream, release_name: &str) {
    enrich_stream_meta(&mut stream.meta, release_name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_dub_and_sub_markers() {
        let mut meta = StreamMeta::default();
        enrich_stream_meta(&mut meta, "Frieren S01E01 (AnimeKai DUB)");
        assert_eq!(meta.dubbed, Some(true));
        assert_eq!(meta.subbed, None);

        let mut meta = StreamMeta::default();
        enrich_stream_meta(&mut meta, "One.Piece.1080p.WEB-DL.AAC2.0.H.264-SUB");
        assert_eq!(meta.subbed, Some(true));

        // A provider's explicit answer is never overwritten.
        let mut meta = StreamMeta {
            dubbed: Some(false),
            ..StreamMeta::default()
        };
        enrich_stream_meta(&mut meta, "Naruto DUBBED 720p");
        assert_eq!(meta.dubbed, Some(false));
    }

    #[test]
    fn folds_release_name_metadata() {
        let mut meta = StreamMeta::default();
        enrich_stream_meta(&mut meta, "The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1");
        assert_eq!(meta.resolution, Some(1080));
        assert_eq!(meta.quality.as_deref(), Some("BluRay"));
        assert_eq!(meta.codec.as_deref(), Some("x264"));
        assert!(meta.audio.contains(&"DTS Lossless".to_string()));
    }

    #[test]
    fn keeps_provider_metadata() {
        let mut meta = StreamMeta {
            resolution: Some(2160),
            quality: Some("WEB-DL".into()),
            ..StreamMeta::default()
        };
        enrich_stream_meta(&mut meta, "The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1");
        assert_eq!(meta.resolution, Some(2160));
        assert_eq!(meta.quality.as_deref(), Some("WEB-DL"));
        // Untouched fields still fill in.
        assert_eq!(meta.codec.as_deref(), Some("x264"));
    }

    #[test]
    fn parses_language_tags() {
        let mut meta = StreamMeta::default();
        enrich_stream_meta(&mut meta, "Show.S01.1080p.WEB-DL.DDP5.1.Atmos.H.264-Multi");
        assert!(meta.languages.contains(&CountryCode::Multi));
    }
}
