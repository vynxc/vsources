//! Episodes handlers (TS `handlers.ts` lines 1566-2001, "Batch 8"),
//! including the fallback episode-detection process closure.

use crate::processors::Processor;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Episodes handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "episodes",
            r"(?:[\W\d]|^)e[ .]?[(\[]?(\d{1,3}(?:[à .-]*(?:[&+]|e){1,2}[ .]?\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:[\W\d]|^)ep[ .]?[(\[]?(\d{1,3}(?:[ .-]*(?:[&+]|ep){1,2}[ .]?\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:[\W\d]|^)\d+[xх][ .]?[(\[]?(\d{1,3}(?:[ .]?[xх][ .]?\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:[\W\d]|^)(?:episodes?|[Сс]ерии:?)[ .]?[(\[]?(\d{1,3}(?:[ .+]*[&+][ .]?\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"[(\[]?(?:\D|^)(\d{1,3}[ .]?ao[ .]?\d{1,3})[)\]]?(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:[\W\d]|^)(?:e|eps?|episodes?|[Сс]ерии:?|\d+[xх])[ .]*[(\[]?(\d{1,3}(?:-\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"\bs\d{1,2}[ .]*-[ .]*\b(\d{1,3}(?:[ .]*~[ .]*\d{1,3})+)\b",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:so?|t)\d{1,4}[. ]?[xх-]?[. ]?(?:e|x|х|ep)[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\D|$)",
        )
        .with_remove()
        .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"(?:so?|t)\d{1,2}\s?[-.]\s?(\d{1,4})(?:[abc]|v0?[1-4]|\D|$)",
        )
        .with_transform(Transform::IntArray),
        Handler::new("episodes", r"\b(?:so?|t)\d{2}(\d{2})\b")
            .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"(?:\W|^)(\d{1,3}(?:[ .]*~[ .]*\d{1,3})+)(?:\W|$)",
        )
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"-\s(\d{1,3}[ .]*-[ .]*\d{1,3})(?:-\d*)?(?:\W|$)",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"-\s(\d{1,3}[ .]*-[ .]*\d{1,3})(?:-\d*)")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"s\d{1,2}\s?\((\d{1,3}[ .]*-[ .]*\d{1,3})\)",
        )
        .with_transform(Transform::IntRange),
        Handler::new_case_sensitive(
            "episodes",
            r"(?:^|\/)(?:20-20)?\d{1,2}-(\d{2})\b(?:-\d)?",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"^(?:20-20)|\d{1,2}-(\d{2})\b(?:-\d)")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntArray),
        Handler::new_case_sensitive(
            "episodes",
            r"(?:\d-)?\b\d{1,2}-(\d{2})(?:\.\w{2,4}$)",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"(?:\d-)\b\d{1,2}-(\d{2})")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"(?:^\[.+].+)([. ]+-[. ]*(\d{1,4})[. ]+)(?:\W)",
        )
        .with_transform(Transform::IntArray)
        .with_value_group(2)
        .with_match_group(1),
        Handler::new(
            "episodes",
            r"(?:(?:seasons?|[Сс]езони?)\P{L}*)?(?:[ .(\[-]|^)(\d{1,3}(?:[ .]?[,&+~][ .]?\d{1,3})+)(?:[ .)\]-]|$)",
        )
        .with_validator(Validator::And(vec![
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:(?:seasons?|[Сс]езони?)\P{L}*)")
                .unwrap_or_else(crate::js_regex::never_match),
            ),
            // Reject captures that are fragments of decimal figures: the
            // capture must not be preceded by "N." nor followed by ".N".
            Validator::Or(vec![
                Validator::lookbehind(r"\d", true, false),
                Validator::NotMatch(
                    crate::js_regex::compile_ci(r"^\.")
                        .unwrap_or_else(crate::js_regex::never_match),
                ),
            ]),
            Validator::Or(vec![
                Validator::NotMatch(
                    crate::js_regex::compile_ci(r"\.$")
                        .unwrap_or_else(crate::js_regex::never_match),
                ),
                Validator::lookahead(r"^\d", true, false),
            ]),
        ]))
        .with_transform(Transform::IntRange),
        Handler::new(
            "episodes",
            r"(?:(?:seasons?|[Сс]езони?)\P{L}*)?(?:20-20)?(?:[ .(\[-]|^)(\d{1,4}(?:-\d{1,4})+)(?:[ .)(\]]|[+-]\D|$)",
        )
        .with_validator(Validator::And(vec![
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:seasons?|[Сс]езони?)\P{L}*|^(?:20-20)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
            Validator::Or(vec![
                Validator::lookbehind(r"Tatsuki[\s._-]Fujimoto", true, false),
                Validator::NotMatch(
                    crate::js_regex::compile_ci(r"\b17-26\b")
                        .unwrap_or_else(crate::js_regex::never_match),
                ),
            ]),
        ]))
        .with_transform(Transform::IntRange),
        // "Season 3-01" is season 3 episode 1 — unless a multi-season range
        // already matched, in which case this is a season range.
        Handler::new(
            "episodes",
            r"\bseason[. ]?\d{1,2}[. ]?-[. ]?(\d{1,3})(?:\D|$)",
        )
        .with_transform(Transform::EpisodeUnlessSeasonRange),
        Handler::new(
            "episodes",
            r"\bEp(?:isode)?\W+\d{1,2}\.(\d{1,3})\b",
        )
        .with_transform(Transform::IntArray),
        Handler::new("episodes", r"Ep.\d+.-.\d+")
            .with_transform(Transform::IntRange)
            .with_remove(),
        Handler::new("episodes", r"(\d{1,3})[. ]?(?:of|из|iz)[. ]?\d{1,3}")
            .with_validator(Validator::And(vec![
                Validator::lookbehind(r"(?:\D|^)", true, true),
                Validator::lookahead(r"(?:\D|$)", true, true),
            ]))
            .with_transform(Transform::IntRangeTill),
        // Spanish "Cap.205" / "Cap.1901_1909" is a season+episode composite:
        // SSEE (s2e05, s19e01-09).
        Handler::new(
            "episodes",
            r"\bcaa?p(?:itulo)?s?[. ]?(\d{1,2})(\d{2})(?:[ _-](\d{1,2})(\d{2}))?\b",
        )
        .with_remove()
        .with_transform(Transform::SpanishCapEpisode),
        Handler::new(
            "episodes",
            r"(?:\b[ée]p?(?:isode)?|[Ээ]пизод|[Сс]ер(?:ии|ия|\.)?|caa?p(?:itulo)?|epis[oó]dio)[. ]?[-:#№]?[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\W|$)",
        )
        .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"\b(\d{1,3})(?:-?я)?[ ._-]*(?:ser(?:i?[iyj]a|\b)|[Сс]ер(?:ии|ия|\.)?)",
        )
        .with_transform(Transform::IntArray),
        // Reject matches that begin inside a decimal figure (the ".0x3" of
        // "2.0x3"): the match must not start with "." right after a digit.
        Handler::new(
            "episodes",
            r"(?:\D|^)\d{1,2}[. ]?[Xxх][. ]?(\d{1,3})(?:[abc]|v0?[1-4]|\D|$)",
        )
        .with_validator(Validator::Or(vec![
            Validator::lookbehind(r"\d", true, false),
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"^\.")
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
        ]))
        .with_transform(Transform::IntArray),
        Handler::new("episodes", r"[\[(]\d{1,2}\.(\d{1,3})[)\]]")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive(
            "episodes",
            r"\b[Ss](?:eason\W?)?\d{1,2}[ .](\d{1,2})\b",
        )
        .with_transform(Transform::IntArray),
        Handler::new("episodes", r"-\s?\d{1,2}\.(\d{2,3})\s?-")
            .with_transform(Transform::IntArray),
        Handler::new_case_sensitive("episodes", r"^\d{1,2}\.(\d{2,3}) - ")
            .with_skip_if_before(&["year", "source", "resolution"])
            .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"\b\d{2}[ ._-](\d{2})(?:.F)?\.\w{2,4}$",
        )
        .with_transform(Transform::IntArray),
        Handler::new(
            "episodes",
            r"(?:^)?\[(\d{2,3})](?:(?:\.\w{2,4})?$)?",
        )
        .with_validator(Validator::And(vec![
            Validator::NotAtStart,
            Validator::NotAtEnd,
            Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:720|1080)|\[(\d{2,3})](?:(?:\.\w{2,4})$)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ),
        ]))
        .with_transform(Transform::IntArray),
        Handler::new("episodes", r"\bodc[. ]+(\d{1,3})\b")
            .with_transform(Transform::IntArray),
        // x264/x265 codec numbers are not episodes: reject when a standalone
        // "x"/"h" appears anywhere before the match.
        Handler::new("episodes", r"\b264\b|\b265\b")
            .with_validator(Validator::lookbehind(r"\b[xh]\b[\s\S]*", true, false))
            .with_transform(Transform::IntArray)
            .with_remove(),
        Handler::new(
            "episodes",
            r"(?:\W|^)(?:\d+)?(?:e|ep)(\d{1,3})(?:\W|$)",
        )
        .with_transform(Transform::IntArray)
        .with_remove(),
        Handler::new("episodes", r"\d+.-.\d+TV")
            .with_transform(Transform::IntRange)
            .with_remove(),
        Handler::new(
            "episodes",
            r"season\s*\d{1,2}\s*(\d{1,4}\s*-\s*\d{1,4})",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"season\s*\d{1,2}\s*-")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::IntRange),
        // Fallback episode detection from dash/bracket markers, trailing
        // numbers, and bare leading numbers before the technical fields.
        Handler::process_only("episodes", Processor::EpisodesFallback),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parser::parse_torrent_title;

    /// Cases taken from the TypeScript `episodes.test.ts` corpus; only the
    /// `episodes` field (this file's field) is asserted.
    #[test]
    fn episodes_table_cases() {
        let cases: &[(&str, i64)] = &[
            ("The Simpsons S28E21 720p HDTV x264-AVS", 21),
            ("breaking.bad.s01e01.720p.bluray.x264-reward", 1),
            ("Vikings.s02.09.AVC.tahiy.mkv", 9),
            ("The.Witcher.S01.07.2019.Dub.AVC.ExKinoRay.mkv", 7),
            (
                "One.Piece.S01E1116.Lets.Go.Get.It!.Buggys.Big.Declaration.2160p.B-Global.WEB-DL.JPN.AAC2.0.H.264.MSubs-ToonsHub.mkv",
                1116,
            ),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.episodes, Some(vec![*expected]), "title: {title}");
        }
    }
}
