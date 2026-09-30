//! Complete handlers (TS `handlers.ts` lines 1242-1335).

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Complete handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("complete", r"\b(?:INTEGRALE?|INTÉGRALE?)\b")
            .with_transform(Transform::Boolean)
            .with_keep_matching()
            .with_remove(),
        Handler::new(
            "complete",
            r"(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bbox[ .-]?set\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new(
            "complete",
            r"(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bmini[ .-]?series\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new(
            "complete",
            r"(?:\bthe\W)?(?:\bcomplete|full|\ball)\b.*\b(?:series|seasons|collection|episodes|set|pack|movies)\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new(
            "complete",
            r"\b(?:series|seasons|movies?)\b.*\b(?:complete|collection)\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new(
            "complete",
            r"(?:\bthe\W)?\bultimate\b[ .]\bcollection\b",
        )
        .with_transform(Transform::Boolean)
        .with_keep_matching(),
        Handler::new(
            "complete",
            r"\bcollection\b.*\b(?:set|pack|movies)\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new("complete", r"\bcollection(?:(\s\[|\s\())")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("complete", r"\bkolekcja\b(?:\Wfilm(?:y|ów|ow)?)?")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new(
            "complete",
            r"duology|trilogy|quadr[oi]logy|tetralogy|pentalogy|hexalogy|heptalogy|anthology",
        )
        .with_transform(Transform::Boolean)
        .with_keep_matching(),
        Handler::new("complete", r"\bcompleta\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("complete", r"\bsaga\b")
            .with_transform(Transform::Boolean)
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("complete", r"\b\[Complete\]\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new("complete", r"(?:A.?|The.?)?\bComplete\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:A.?|The.?)\bComplete")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::new_case_sensitive("complete", r"\bCOMPLETE\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
    ]
}
