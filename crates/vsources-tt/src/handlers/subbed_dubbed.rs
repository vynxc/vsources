//! Subbed, dubbed, and multi-language detection handlers
//! (TS `handlers.ts` lines 2002-2072, "Batch 9").

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Subbed/dubbed/multi-language handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "quality",
            r"\b(?:H[DQ][ .-]*)?CAM(?:H[DQ])?(?:[ .-]*Rip)?\b",
        )
        .with_transform(Transform::Value("CAM".to_string()))
        .with_skip_if_first()
        .with_remove()
        .with_retry_past_title(r"^cam$"),
        Handler::new(
            "quality",
            r"\b(?:\w.)?WEB\b|\bWEB(?:(?:[ \.\-\(\],]+\d))?\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"\b(?:\w.)WEB\b|\bWEB(?:(?:[ \.\-\(\],]+\d))\b")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::Value("WEB".to_string()))
        .with_remove()
        .with_skip_from_title()
        .with_retry_past_title(r"^web$"),
        Handler::new_case_sensitive("country", r"\b(US|UK|AU|NZ)\b").with_must_end_title(),
        Handler::new(
            "editions",
            r"\b(?:custom.?)?Extended(?:[\.\s\-\+_\/(),](?:Editions?|Cut))?\b",
        )
        .with_transform(Transform::ValueSet("Extended Edition".to_string()))
        .with_keep_matching()
        .with_remove()
        .with_must_end_title(),
        Handler::new(
            "subbed",
            r"\bSUB(?:FRENCH)\b|\b(?:DAN|E|FIN|PL|SLO|SWE)SUBS?\b",
        )
        .with_transform(Transform::Boolean),
        Handler::new(
            "languages",
            r"\bmulti(?:ple)?[ .-]*(?:su?$|sub\w*|dub\w*)\b|msub",
        )
        .with_transform(Transform::ValueSet("multi subs".to_string()))
        .with_keep_matching()
        .with_remove(),
        Handler::new(
            "languages",
            r"\bmulti(?:ple)?[ .-]*(?:lang(?:uages?)?|audio|VF2)?\b",
        )
        .with_transform(Transform::ValueSet("multi audio".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\btri(?:ple)?[ .-]*(?:audio|dub\w*)\b")
            .with_transform(Transform::ValueSet("multi audio".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bdual[ .-]*(?:au?$|[aá]udio|line)\b")
            .with_transform(Transform::ValueSet("dual audio".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bdual\b(?:[ .-]*sub)?")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:[ .-]*sub)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("dual audio".to_string()))
            .with_keep_matching(),
    ]
}
