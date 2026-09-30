//! Extended and Edition handlers
//! (TS `handlers.ts` lines 350-467; `handlers.go` lines 544-606).

use crate::js_regex;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Extended and Edition handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // Extended handlers (lines 544-549 in handlers.go)
        Handler::new_case_sensitive("extended", r"EXTENDED").with_transform(Transform::Boolean),
        Handler::new("extended", r"- Extended").with_transform(Transform::Boolean),
        // Edition handlers (lines 551-606 in handlers.go)
        Handler::new(
            "editions",
            r"\b\d{2,3}(?:th)?[\.\s\-\+_\/(),]Anniversary[\.\s\-\+_\/(),](?:Edition|Ed)?\b",
        )
        .with_transform(Transform::ValueSet("Anniversary Edition".to_string()))
        .with_keep_matching()
        .with_remove()
        .with_skip_if_before(&["year"]),
        Handler::new("editions", r"\b(?:D(ragon)?[\.\s\-\+_\/(),]?Box)\b")
            .with_transform(Transform::ValueSet("Dragon Box".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bCC\b|\bcolou?r[.\s-]?corrected\b")
            .with_transform(Transform::ValueSet("Color Corrected".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bUltimate[\.\s\-\+_\/(),]Edition\b")
            .with_transform(Transform::ValueSet("Ultimate Edition".to_string()))
            .with_keep_matching()
            .with_remove(),
        // Criterion collection
        Handler::new("editions", r"\bCriterion\.Collection\b")
            .with_transform(Transform::ValueSet("Criterion Collection".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bDirector\W?s.?Cut\b")
            .with_transform(Transform::ValueSet("Director's Cut".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bCollector\W?s\b")
            .with_transform(Transform::ValueSet("Collector's Edition".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bTheatrical\b")
            .with_transform(Transform::ValueSet("Theatrical".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\buncut(?:.gems)?\b")
            .with_validator(Validator::NotMatch(
                js_regex::compile_ci(r"(?:.gems)").unwrap_or_else(js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("Uncut".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\bIMAX\b")
            .with_transform(Transform::ValueSet("IMAX".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_from_title(),
        Handler::new("editions", r"\bDiamond[\s.]Edition\b")
            .with_transform(Transform::ValueSet("Diamond Edition".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("editions", r"\b\.Diamond\.\b")
            .with_transform(Transform::ValueSet("Diamond Edition".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new(
            "editions",
            r"\bRemaster(?:ed)?\b|\b[\[(]?REKONSTRUKCJA[\])]?\b",
        )
        .with_transform(Transform::ValueSet("Remastered".to_string()))
        .with_keep_matching()
        .with_remove(),
        Handler::new_case_sensitive("editions", r"\bDC\b")
            .with_transform(Transform::ValueSet("Director's Cut".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_if_before(&["year"]),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Table-driven cases from `edition.test.ts`; only fields owned by this
    /// section are asserted (other sections may still be stubbed).
    #[test]
    fn editions_are_detected() {
        let cases: &[(&str, Option<&[&str]>)] = &[
            // "Anniversary Edition"
            (
                "Mary.Poppins.1964.50th.ANNIVERSARY.EDITION.REMUX.1080p.Bluray.AVC.DTS-HD.MA.5.1-LEGi0N",
                Some(&["Anniversary Edition"]),
            ),
            // "Color Corrected CC" — DBOX is the Dragon Box edition.
            (
                "Dragon.Ball.001.DBOX.CC.480p.x264-SoM.mkv",
                Some(&["Dragon Box", "Color Corrected"]),
            ),
            // "Multiple editions"
            (
                "Some.Movie.2020.IMAX.REMASTERED.1080p.BluRay.x264",
                Some(&["IMAX", "Remastered"]),
            ),
            // "Director's Cut"
            (
                "Basic.Instinct.1992.Unrated.Directors.Cut.Bluray.1080p.DTS-HD-HR-6.1.x264-Grym@BTNET",
                Some(&["Director's Cut"]),
            ),
            // "No edition for Uncut Gems" — the validator rejects the form.
            ("Uncut.Gems.2019.1080p.NF.WEB-DL.DDP5.1.x264-NTG", None),
        ];
        for (input, expected) in cases {
            let parsed = parse_torrent_title(input);
            let actual: Option<Vec<&str>> = parsed
                .editions
                .as_ref()
                .map(|editions| editions.iter().map(String::as_str).collect());
            assert_eq!(actual.as_deref(), *expected, "input: {input}");
        }
    }

    /// "Extended Edition LOTR" — the `extended` flag is set by this
    /// section's case-sensitive handler.
    #[test]
    fn extended_flag_is_set() {
        let parsed = parse_torrent_title(
            "The.Lord.of.the.Rings.The.Fellowship.of.the.Ring.2001.EXTENDED.2160p.UHD.BluRay.x265.10bit.HDR.TrueHD.7.1.Atmos-BOREDOR",
        );
        assert_eq!(parsed.extended, Some(true));
    }
}
