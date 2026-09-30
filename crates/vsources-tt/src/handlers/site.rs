//! Site handlers (TS `handlers.ts` lines 80-102).

use crate::types::Handler;

/// Site handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "site",
            r"^(www?[., ][\w-]+[. ][\w-]+(?:[. ][\w-]+)?)\s+-\s*",
        )
        .with_keep_matching()
        .with_skip_from_title()
        .with_remove(),
        Handler::new("site", r"\bwww[., ][\w-]+[., ](?:rodeo|hair)\b")
            .with_remove()
            .with_skip_from_title(),
    ]
}
