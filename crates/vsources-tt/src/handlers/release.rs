//! Release Types, Upscaled, Convert, Hardcoded, Proper, Repack, Retail,
//! Documentary, Unrated, Uncensored, Commentary, and Region handlers
//! (TS `handlers.ts` lines 468-625; `handlers.go` lines 608-710).

use crate::transforms::Transform;
use crate::types::Handler;

/// Release handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // Release Types handlers (lines 608-623 in handlers.go)
        Handler::new(
            "releaseTypes",
            r"\b((?:OAD|OAV|ODA|ONA|OVA)\b(?:[+&]\b(?:OAD|OAV|ODA|ONA|OVA)\b)?)",
        )
        .with_transform(Transform::ValueSetMultiUppercase)
        .with_remove()
        .with_match_group(1),
        Handler::new(
            "releaseTypes",
            r"\b(OAD|OAV|ODA|ONA|OVA)(?:[ .-]*\d{1,3})?(?:v\d)?\b",
        )
        .with_transform(Transform::ValueSetTransform(true))
        .with_remove()
        .with_match_group(1),
        Handler::new_case_sensitive("releaseTypes", r"\b(?:[Ww]ith[ .])?ASL\b")
            .with_transform(Transform::ValueSet("ASL".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new(
            "releaseTypes",
            r"\b(?:with[ .])?(?:Audio[ .]Description|Descriptive[ .]Audio)\b",
        )
        .with_transform(Transform::ValueSet("Audio Description".to_string()))
        .with_keep_matching()
        .with_remove(),
        // Upscaled handlers (lines 625-636 in handlers.go)
        Handler::new("regraded", r"\bRegraded?\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("upscaled", r"\b(?:AI.?)?(Upscal(ed?|ing)|Enhanced?)\b")
            .with_transform(Transform::Boolean),
        Handler::new("upscaled", r"\b(?:iris2|ups(?:uhd|fhd|hd|4k))\b")
            .with_transform(Transform::Boolean),
        Handler::new("upscaled", r"\bups\b")
            .with_transform(Transform::Boolean)
            .with_skip_if_first(),
        Handler::new("upscaled", r"\b\.AI\.\b").with_transform(Transform::Boolean),
        // Convert handler (lines 638-643 in handlers.go)
        Handler::new_case_sensitive("convert", r"\bCONVERT\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Hardcoded handler (lines 645-650 in handlers.go)
        Handler::new_case_sensitive("hardcoded", r"\b(?:HC|HARDCODED)\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("preair", r"\bPRE[ .-]?AIR(?:ED)?\b").with_remove(),
        // Proper handler (lines 652-657 in handlers.go)
        Handler::new("proper", r"\b(?:REAL.)?PROPER\d?\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Repack handler (lines 659-664 in handlers.go)
        Handler::new("repack", r"\b(?:REPACK|RERIP)\d?\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Retail handler (lines 666-670 in handlers.go)
        Handler::new("retail", r"\bRetail\b").with_transform(Transform::Boolean),
        // Documentary handler (lines 672-677 in handlers.go)
        Handler::new("documentary", r"\bDOCU(?:menta?ry)?\b")
            .with_transform(Transform::Boolean)
            .with_skip_from_title(),
        // Unrated handler (lines 679-684 in handlers.go)
        Handler::new("unrated", r"\bunrated\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Uncensored handler (lines 686-691 in handlers.go)
        Handler::new("uncensored", r"\buncensored\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Commentary handler (lines 693-698 in handlers.go)
        Handler::new("commentary", r"\bcommentary\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        // Region handlers (lines 700-710 in handlers.go)
        Handler::new_case_sensitive("region", r"R\dJ?\b")
            .with_remove()
            .with_skip_if_first(),
        Handler::new_case_sensitive("region", r"\b(PAL|NTSC|SECAM)\b")
            .with_transform(Transform::Uppercase)
            .with_remove(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;
    use crate::types::ExtraValue;

    /// `releaseTypes` cases from `episodeTitle.test.ts` and `seasons.test.ts`;
    /// only fields owned by this section are asserted.
    #[test]
    fn release_types_are_detected() {
        let cases: &[(&str, &[&str])] = &[
            (
                "House.of.the.Dragon.S01E03.Second.of.His.Name.with.ASL.1080p.AMZN.WEB-DL.DDP5.1.H.264-Kitsune",
                &["ASL"],
            ),
            (
                "Silo.S01E04.Truth.with.Audio.Description.1080p.ATVP.WEB-DL.DDP5.1.Atmos.H.264-Kitsune",
                &["Audio Description"],
            ),
            // "OVA&ODA" is split on non-alphanumerics and uppercased.
            (
                "[Anime Time] One Punch Man [S1+S2+OVA&ODA][Dual Audio][1080p BD][HEVC 10bit x265][AAC][Eng Subs]",
                &["OVA", "ODA"],
            ),
            (
                "DARKER THAN BLACK - S00E04 - Darker Than Black Gaiden OVA 3.mkv",
                &["OVA"],
            ),
        ];
        for (input, expected) in cases {
            let parsed = parse_torrent_title(input);
            let actual: Option<Vec<&str>> = parsed
                .release_types
                .as_ref()
                .map(|types| types.iter().map(String::as_str).collect());
            assert_eq!(actual.as_deref(), Some(*expected), "input: {input}");
        }
    }

    /// Boolean release flags from `group.test.ts` and `resolution.test.ts`.
    #[test]
    fn release_flags_are_detected() {
        let cases: &[(&str, &str)] = &[
            (
                "The.Expanse.S05E02.PROPER.720p.WEB.h264-KOGi[rartv]",
                "proper",
            ),
            (
                "Annabelle.2014.1080p.PROPER.HC.WEBRip.x264.AAC.2.0-RARBG",
                "hardcoded",
            ),
            (
                "Better.Call.Saul.S03E04.CONVERT.720p.WEB.h264-TBS",
                "convert",
            ),
        ];
        for (input, field) in cases {
            let parsed = parse_torrent_title(input);
            let value = match *field {
                "proper" => parsed.proper,
                "hardcoded" => parsed.hardcoded,
                _ => parsed.convert,
            };
            assert_eq!(value, Some(true), "field {field:?}, input: {input}");
        }
    }

    /// `regraded` from `edition.test.ts` ("Regraded") — a regrade is not an
    /// upscale.
    #[test]
    fn regraded_is_detected_without_upscaling() {
        let parsed = parse_torrent_title(
            "Dragon.Ball.Z.013.480p.DBox.DVD.REGRADE.Dual-Audio.FLAC2.0.x264-SoM.mkv",
        );
        assert_eq!(parsed.regraded, Some(true));
        assert_eq!(parsed.upscaled, None);
    }

    /// `preair` from `episodeTitle.test.ts` — a non-standard field, so the
    /// matched tag lands in `extra`.
    #[test]
    fn preair_is_captured_as_a_release_tag() {
        let parsed = parse_torrent_title("The.Office.IL.S01E01.PREAiR.HDTV.XviD-MaxHD");
        assert_eq!(
            parsed.extra.get("preair"),
            Some(&ExtraValue::Str("PREAiR".to_string()))
        );
    }
}
