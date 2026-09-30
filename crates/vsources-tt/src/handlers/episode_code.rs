//! Episode-code handlers plus the early 20-40 group handler
//! (TS `handlers.ts` lines 103-119).

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Episode-code handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // 20-40 group handler, placed before episode handlers to prevent
        // matching as episodes.
        Handler::new("group", r"\b(20-40)\b$")
            .with_transform(Transform::Value("20-40".to_string()))
            .with_remove(),
        Handler::new_case_sensitive(
            "episodeCode",
            r"([\[(]([a-z0-9]{8}|[A-Z0-9]{8})[\])])(?:\.[a-zA-Z0-9]{1,5}$|$)",
        )
        .with_transform(Transform::Uppercase)
        .with_remove()
        .with_match_group(1)
        .with_value_group(2),
        Handler::new_case_sensitive("episodeCode", r"\[([A-Z0-9]{8})]")
            .with_validator(Validator::Match(
                crate::js_regex::compile_ci(r"(?:[A-Z]+\d|\d+[A-Z])")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::Uppercase)
            .with_remove(),
    ]
}
