//! Date handlers (TS `handlers.ts` lines 191-266).

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Date handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // Y M D with a four-digit year, e.g. `2019 10 25`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W|^)([(\[]?((?:19[6-9]|20[012])[0-9]([. \-/\\])(?:0[1-9]|1[012])([. \-/\\])(?:0[1-9]|[12][0-9]|3[01]))[)\]]?)(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![3, 4]))
        .with_transform(Transform::Date("2006 01 02".to_string()))
        .with_remove()
        .with_value_group(2)
        .with_match_group(1),
        // D M Y with a four-digit year, e.g. `25 10 2019`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W|^)[(\[]?((?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])([. \-/\\])(?:19[6-9]|20[012])[0-9])[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::Date("02 01 2006".to_string()))
        .with_remove(),
        // M D Y with a four-digit year, e.g. `10 30 2010`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W)[(\[]?((?:0[1-9]|1[012])([. \-/\\])(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:19[6-9]|20[012])[0-9])[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::Date("01 02 2006".to_string()))
        .with_remove(),
        // M D Y with a two-digit year, e.g. `11/21/17`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W)[(\[]?((?:0[1-9]|1[012])([. \-/\\])(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:[0][1-9]|[0126789][0-9]))[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::Date("01 02 06".to_string()))
        .with_remove(),
        // D M Y with a two-digit year, e.g. `18.09.00`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W)[(\[]?((?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])([. \-/\\])(?:[0][1-9]|[0126789][0-9]))[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::Date("02 01 06".to_string()))
        .with_remove()
        .with_match_group(1),
        // D MMM Y, e.g. `16-Feb-2017` or `9th Dec 2019`.
        Handler::new(
            "date",
            r"(?:\W|^)[(\[]?((?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)([. \-/\\])(?:19[7-9]|20[012])[0-9])[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::DateCombo("_2 Jan 2006".to_string()))
        .with_remove(),
        // D MMM YY, e.g. `16-Feb-17`.
        Handler::new(
            "date",
            r"(?:\W|^)[(\[]?((?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)([. \-/\\])(?:0[1-9]|[0126789][0-9]))[)\]]?(?:\W|$)",
        )
        .with_validator(Validator::MatchedGroupsAreSame(vec![2, 3]))
        .with_transform(Transform::DateCombo("_2 Jan 06".to_string()))
        .with_remove(),
        // Compact YYYYMMDD, e.g. `20200116`.
        Handler::new_case_sensitive(
            "date",
            r"(?:\W|^)[(\[]?(20[012][0-9](?:0[1-9]|1[012])(?:0[1-9]|[12][0-9]|3[01]))[)\]]?(?:\W|$)",
        )
        .with_transform(Transform::Date("20060102".to_string()))
        .with_remove(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases taken from the TypeScript `date.test.ts`.
    #[test]
    fn parses_date() {
        let cases: &[(&str, Option<&str>)] = &[
            // Y M D (handler 1).
            (
                "Stephen Colbert 2019 10 25 Eddie Murphy 480p x264-mSD [eztv]",
                Some("2019-10-25"),
            ),
            // D MMM Y with an ordinal day suffix (handler 6).
            (
                "SIX.S01E05.400p.229mb.hdtv.x264-][ Collateral ][ 16-Feb-2017 mp4",
                Some("2017-02-16"),
            ),
            (
                "WWE RAW 9th Dec 2019 WEBRip h264-TJ [TJET]",
                Some("2019-12-09"),
            ),
            // D M Y with a two-digit year (handler 5).
            ("wwf.raw.is.war.18.09.00.avi", Some("2000-09-18")),
            // Mixed separators are not a date.
            ("11-11-11.2011.1080p.BluRay.x264.DTS-FGT", None),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.date.as_deref(), *expected, "title: {title}");
        }
    }
}
