//! Seasons handlers (TS `handlers.ts` lines 1336-1565, "Batch 7").

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Seasons handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "seasons",
            r"(?:complete\W|seasons?\W|\W|^)((?:s\d{1,2}[., +/\\&-]+)+s\d{1,2}\b)",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"(?:complete\W|seasons?\W|\W|^)[(\[]?(s\d{2,}-\d{2,}\b)[)\]]?",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"(?:complete\W|seasons?\W|\W|^)[(\[]?(s[1-9]-[2-9]\b)[)\]]?",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"\d+ª(?:.+)?(?:a.?)?\d+ª(?:(?:.+)?(?:temporadas?))",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons?|[Сс]езони?|sezon|temporadas?|stagioni)[. ]?[-:]?[. ]?[(\[]?((?:\d{1,2} ?(?:[,/\\&]+ ?)+)+\d{1,2}\b)[)\]]?",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons|[Сс]езони?|sezon|temporadas?|stagioni)[. ]?[-:]?[. ]?[(\[]?((?:\d{1,2}[. -]+)+0?[1-9]\d?\b)[)\]]?",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        // "Season 1-6"-style ranges. The TS validator rejects titles ending
        // in a file extension (the pattern's optional trailing group pulls
        // the extension into the match), captures containing 2+ consecutive
        // spaces, and anime-style "[Group] Title Season 3 - 14" where the
        // spaced dash separates the season from a 2+ digit episode number.
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?season[. ]?[(\[]?((?:\d{1,2}[. -]+)+0?\d{1,2}\b)[)\]]?(?:.*\.\w{2,4}$)?",
        )
        .with_validator(Validator::And(vec![
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:.*\.\w{2,4}$)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
            Validator::NotMatch(
                crate::js_regex::compile(r"\s{2,}", false)
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
            Validator::Or(vec![
                Validator::lookbehind(r"^\[[^\]]+\].*", false, false),
                Validator::NotMatch(
                    crate::js_regex::compile_ci(r"season[. ]?\d{1,2} - \d{2,4}[)\]]?$")
                        .unwrap_or_else(crate::js_regex::never_match),
                ),
            ]),
        ]))
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?\bseasons?\b[. -]?(\d{1,2}[. -]?(?:to|thru|and|\+|:)[. -]?\d{1,2})\b",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        Handler::new(
            "seasons",
            r"\bseason\b[ .-]?(\d{1,2}[ .-]?(?:to|thru|and|\+)[ .-]?\bseason\b[ .-]?\d{1,2})",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "seasons",
            r"(\d{1,2})(?:-?й)?[. _]?(?:[Сс]езон|sez(?:on)?)(?:\P{L}?\D|$)",
        )
        .with_transform(Transform::IntArray)
        .with_remove(),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?(?:saison|seizoen|sezon(?:SO?)?|stagione|season|series|temp(?:orada)?):?[. ]?(\d{1,2})",
        )
        .with_transform(Transform::IntArray),
        Handler::new("seasons", r"[Сс]езон:?[. _]?№?(\d{1,2})(?:\d)?")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"\d{3}")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::IntArray)
            .with_remove(),
        Handler::new("seasons", r"(?:\D|^)(\d{1,2})Â?[°ºªa]?[. ]*temporada")
            .with_transform(Transform::IntArray)
            .with_remove(),
        // Spanish "Cap.205" / "Cap.1901_1909" is a season+episode composite:
        // SSEE (s2e05, s19e01-09).
        Handler::new(
            "seasons",
            r"\bcaa?p(?:itulo)?s?[. ]?(\d{1,2})\d{2}(?:[ _-]\d{1,4})?\b",
        )
        .with_transform(Transform::IntArray),
        Handler::new("seasons", r"\bt(\d{1,3})(?:[ex]+|$)")
            .with_transform(Transform::IntArray)
            .with_remove(),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete)?(?:\W|^)so?([01]?[0-5]?[1-9])(?:[\Wex]|\d{2}\b)",
        )
        .with_validator(Validator::NotStartSpaced)
        .with_transform(Transform::IntArray)
        .with_keep_matching(),
        Handler::new(
            "seasons",
            r"(?:so?|t)(\d{1,4})[. ]?[xх-]?[. ]?(?:e|x|х|ep|-|\.)[. ]?\d{1,4}(?:[abc]|v0?[1-4]|\D|$)",
        )
        .with_transform(Transform::IntArray),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete\W)?(?:\W|^)(\d{1,2})[. ]?(?:st|nd|rd|th)[. ]*season",
        )
        .with_transform(Transform::IntArray),
        // Reject matches that begin inside a decimal figure (the ".0x3" of
        // "2.0x3"): the match must not start with "." right after a digit.
        Handler::new_case_sensitive("seasons", r"(?:\D|^)(\d{1,2})[Xxх]\d{1,3}(?:\D|$)")
            .with_validator(Validator::Or(vec![
                Validator::lookbehind(r"\d", false, false),
                Validator::NotMatch(
                    crate::js_regex::compile(r"^\.", false)
                        .unwrap_or_else(crate::js_regex::never_match),
                ),
            ]))
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("seasons", r"\bSn([1-9])(?:\D|$)")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("seasons", r"[\[(](\d{1,2})\.\d{1,3}[)\]]")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("seasons", r"-\s?(\d{1,2})\.\d{2,3}\s?-")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("seasons", r"^(\d{1,2})\.\d{2,3} - ")
            .with_skip_if_before(&["year", "source", "resolution"])
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive(
            "seasons",
            r"(?:^|\/)(?:20-20)?(\d{1,2})-\d{2}\b(?:-\d)?",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"^(?:20-20)|(\d{1,2})-\d{2}\b(?:-\d)")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("seasons", r"[^\w-](\d{1,2})-\d{2}(?:\.\w{2,4}$)")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive(
            "seasons",
            r"(?:\bEp?(?:isode)? ?\d+\b.*)?\b(\d{2})[ ._]\d{2}(?:.F)?\.\w{2,4}$",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"(?:\bEp?(?:isode)? ?\d+\b.*)")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntArray),
        Handler::new("seasons", r"\bEp(?:isode)?\W+(\d{1,2})\.\d{1,3}\b")
            .with_transform(Transform::IntArray),
        Handler::new(
            "seasons",
            r"(?:(?:\bthe\W)?\bcomplete)?(?:[a-z])?\bs(\d{1,3})(?:[\Wex]|\d{2}\b|$)",
        )
        .with_validator(Validator::And(vec![
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:[a-z])\bs\d{1,3}")
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
            Validator::NotStartSpaced,
        ]))
        .with_transform(Transform::IntArray)
        .with_keep_matching(),
        Handler::new("seasons", r"\bSeasons?\b.*\b(\d{1,2}-\d{1,2})\b")
            .with_transform(Transform::IntRange),
        Handler::new("seasons", r"(?:\W|^)(\d{1,2})(?:e|ep)\d{1,3}(?:\W|$)")
            .with_transform(Transform::IntArray),
        Handler::new("seasons", r"[\[\(]ТВ-(\d{1,2})[\)\]]")
            .with_transform(Transform::IntArray),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parser::parse_torrent_title;

    /// Cases taken from the TypeScript `seasons.test.ts` corpus; only the
    /// `seasons` field (this file's field) is asserted.
    #[test]
    fn seasons_table_cases() {
        let cases: &[(&str, Vec<i64>)] = &[
            ("season 2 of 4", vec![2]),
            (
                "Game Of Thrones Complete Season 1,2,3,4,5,6,7 406p mkv + Subs",
                vec![1, 2, 3, 4, 5, 6, 7],
            ),
            (
                "24 Season 1-8 Complete with Subtitles",
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            ("Naruto Shippuden Season 1:11", (1..=11).collect()),
            ("S011E16.mkv", vec![11]),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.seasons, Some(expected.clone()), "title: {title}");
        }
    }
}
