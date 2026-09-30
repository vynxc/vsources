//! Year handlers (TS `handlers.ts` lines 267-349).

use crate::js_regex;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Year handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // Full year range, e.g. `2001-2011`; a range also marks the release
        // complete.
        Handler::new_case_sensitive(
            "year",
            r"[ .]?([(\[*]?((?:19\d|20[012])\d[ .]?-[ .]?(?:19\d|20[012])\d)[*)\]]?)[ .]?",
        )
        .with_transform(Transform::YearWithComplete)
        .with_match_group(1)
        .with_value_group(2)
        .with_remove(),
        // Year range with a two-digit end, e.g. `1988 - 89`.
        Handler::new_case_sensitive(
            "year",
            r"[(\[*][ .]?((?:19\d|20[012])\d[ .]?-[ .]?\d{2})(?:\s?[*)\]])?",
        )
        .with_transform(Transform::YearWithComplete)
        .with_remove(),
        Handler::new("year", r"[(\[*]?\b(20[0-9]{2}|2100)[*\])]?")
            .with_validator(Validator::lookahead(r"(?:\D*\d{4}\b)", false, false))
            .with_transform(Transform::Year)
            .with_remove(),
        // The TypeScript handler uses a custom validator: the match must
        // start at index >= 2 and capture group 1 must be exactly four
        // characters, i.e. a bare year (`S2015`, `2015kbps`, `Cap.1905`
        // and `20155` are all rejected). Since the whole match is one
        // leading character, the year and an optional `)`/`]`/`*`, that is
        // equivalent to requiring two preceding characters plus a whole
        // match shaped like a single leading character around a bare year.
        Handler::new(
            "year",
            r"(?:[(\[*]|.)((?:\d|[SE]|Cap[. ]?)?(?:19\d|20[012])\d(?:\d|kbps)?)[*)\]]?",
        )
        .with_validator(Validator::And(vec![
            Validator::lookbehind("..", false, true),
            Validator::Match(
                js_regex::compile_ci(r"^.(?:19\d|20[012])\d[*)\]]?$")
                    .unwrap_or_else(js_regex::never_match),
            ),
        ]))
        .with_transform(Transform::Year)
        .with_remove()
        .with_match_group(1),
        // The TypeScript handler uses a custom validator: a bare year at
        // the (start-anchored) match position is rejected, so only a
        // bracketed year at the start of the title passes. This is
        // equivalent to requiring brackets around the four-digit year.
        Handler::new_case_sensitive("year", r"^[(\[]?((?:19\d|20[012])\d)(?:\d|kbps)?[)\]]?")
            .with_validator(Validator::Match(
                js_regex::compile_ci(r"^[\[(](?:19\d|20[012])\d[\])]?$")
                    .unwrap_or_else(js_regex::never_match),
            ))
            .with_transform(Transform::Year)
            .with_remove(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases taken from the TypeScript `year.test.ts`.
    #[test]
    fn parses_year() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "Dawn.of.the.Planet.of.the.Apes.2014.HDRip.XViD-EVO",
                Some("2014"),
            ),
            // The title itself is a year; the later year wins.
            ("2012 2009 1080p BluRay x264 REPACK-METiS", Some("2009")),
            (
                "Harry Potter All Movies Collection 2001-2011 720p Dual KartiKing",
                Some("2001-2011"),
            ),
            (
                "Empty Nest Season 1 (1988 - 89) fiveofseven",
                Some("1988-1989"),
            ),
            // A bare year at the very start is the title, not a year.
            (
                "1923 S02E01 The Killing Season 1080p AMZN WEB-DL DDP5 1 H 264-FLUX[TGx]",
                None,
            ),
            // A Spanish episode code is not a year.
            (
                "Anatomia De Grey - Temporada 19 [HDTV][Cap.1905][Castellano][www.AtomoHD.nu].avi",
                None,
            ),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.year.as_deref(), *expected, "title: {title}");
        }
    }
}
