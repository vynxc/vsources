//! Quality/Source handlers (TS `handlers.ts` lines 626-853, mirroring
//! lines 712-1054 in the original Go `handlers.go`).

use crate::js_regex;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Quality/source handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("quality", r"\b(?:H[DQ][ .-]*)?S[ .-]+print")
            .with_transform(Transform::Value("CAM".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:HD[ .-]*)?T(?:ELE)?S(?:YNC)?(?:Rip)?\b")
            .with_transform(Transform::Value("TeleSync".to_string()))
            .with_remove(),
        Handler::new_case_sensitive("quality", r"\b(?:HD[ .-]*)?T(?:ELE)?C(?:INE)?(?:Rip)?\b")
            .with_transform(Transform::Value("TeleCine".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:DVD?|BD|BR|HD)?[ .-]*Scr(?:eener)?\b")
            .with_transform(Transform::Value("SCR".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bP(?:RE)?-?(HD|DVD)(?:Rip)?\b")
            .with_transform(Transform::Value("SCR".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(Blu[ .-]*Ray)\b(?:.*remux)")
            .with_transform(Transform::Value("BluRay REMUX".to_string()))
            .with_remove()
            .with_match_group(1),
        Handler::new("quality", r"(?:BD|BR|UHD)[- ]?remux")
            .with_transform(Transform::Value("BluRay REMUX".to_string()))
            .with_remove(),
        Handler::new("quality", r"(?:remux.*)\bBlu[ .-]*Ray\b")
            .with_transform(Transform::Value("BluRay REMUX".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bremux\b")
            .with_transform(Transform::Value("REMUX".to_string()))
            .with_remove(),
        // The TS source uses an inline `validateMatch` closure that rejects
        // any match whose text ends with "rip" (case-insensitive); the
        // anchored `NotMatch` pattern expresses the same check so that
        // e.g. `BluRay Rip` falls through to the `BRRip` handler below.
        Handler::new("quality", r"\bBlu[ .-]*Ray\b(?:[ .-]*Rip)?")
            .with_validator(Validator::NotMatch(
                js_regex::compile_ci(r"rip$").unwrap_or_else(js_regex::never_match),
            ))
            .with_transform(Transform::Value("BluRay".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bUHD[ .-]*Rip\b")
            .with_transform(Transform::Value("UHDRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bHD[ .-]*Rip\b")
            .with_transform(Transform::Value("HDRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bMicro[ .-]*HD\b")
            .with_transform(Transform::Value("HDRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:BR|Blu[ .-]*Ray)[ .-]*Rip\b")
            .with_transform(Transform::Value("BRRip".to_string()))
            .with_remove(),
        Handler::new(
            "quality",
            r"\bBD[ .-]*Rip\b|\bBDR\b|\bBD-RM\b|[\[(]BD[\]) .,-]",
        )
        .with_transform(Transform::Value("BDRip".to_string()))
        .with_remove(),
        Handler::new("quality", r"\bVOD[ .-]*Rip\b")
            .with_transform(Transform::Value("VODR".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:HD[ .-]*)?DVD[ .-]*Rip\b")
            .with_transform(Transform::Value("DVDRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bVHS[ .-]*Rip\b")
            .with_transform(Transform::Value("VHSRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:R\d?)?DVD(?:R\d?)?\b")
            .with_transform(Transform::Value("DVD".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bVHS\b")
            .with_transform(Transform::Value("DVD".to_string()))
            .with_remove()
            .with_skip_if_first(),
        Handler::new("quality", r"\bPPV[ .-]*HD\b")
            .with_transform(Transform::Value("PPV".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bPPVRip\b")
            .with_transform(Transform::Value("PPVRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bHD.?TV.?Rip\b")
            .with_transform(Transform::Value("HDTVRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bDVB[ .-]*(?:Rip)?\b")
            .with_transform(Transform::Value("HDTV".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bSAT[ .-]*Rips?\b")
            .with_transform(Transform::Value("SATRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bTVRips?\b")
            .with_transform(Transform::Value("TVRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bR5\b")
            .with_transform(Transform::Value("R5".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bWEB[ .-]*Rip\b")
            .with_transform(Transform::Value("WEBRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bWEB[ .-]?Cap\b")
            .with_transform(Transform::Value("WEBCap".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bWEB[ .-]?DL[ .-]?Rip\b")
            .with_transform(Transform::Value("WEB-DLRip".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bWEB[ .-]*(DL|.BDrip|.DLRIP)\b")
            .with_transform(Transform::Value("WEB-DL".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bWEB[ .-]?HD\b")
            .with_transform(Transform::Value("WEB".to_string()))
            .with_remove(),
        Handler::new("quality", r"\b(?:DL|WEB|BD|BR)MUX\b").with_remove(),
        Handler::new_case_sensitive("quality", r"\b(W(?:ORK)P(?:RINT))\b")
            .with_transform(Transform::Value("WORKPRINT".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bPDTV\b")
            .with_transform(Transform::Value("PDTV".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bHD(?:.?TV)?\b(?!-ELITE\.NET)")
            .with_transform(Transform::Value("HDTV".to_string()))
            .with_remove(),
        Handler::new("quality", r"\bSD(?:.?TV)?\b")
            .with_transform(Transform::Value("SDTV".to_string()))
            .with_remove(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Table-driven cases ported from `quality.test.ts`, asserting only the
    /// `quality` field owned by this module.
    #[test]
    fn quality_matches_ported_corpus() {
        let cases: &[(&str, &str)] = &[
            (
                "Planet.Earth.II.S01.2016.2160p.UHD.BluRay.REMUX.HDR.HEVC.DTS-HD.MA.5.1",
                "BluRay REMUX",
            ),
            (
                "The Monkey King 3 2018 CHINESE 1080p BluRay H264 AAC-VXT",
                "BluRay",
            ),
            (
                "Joker.2019.UHDRip.2160p.HDR.4K.DV.ITA.ENG.Subs.TrueHD.Atmos.7.1.x265-NAHOM",
                "UHDRip",
            ),
            ("Booksmart.2019.BRRip.XviD.AC3-EVO", "BRRip"),
            ("Despicable.Me.2010.FRENCH.BDRip.XviD-SANCTUARY", "BDRip"),
            ("The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1", "BluRay"),
        ];
        for &(title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.quality.as_deref(), Some(expected), "title: {title}");
        }
    }
}
