//! Title handlers (TS `handlers.ts` lines 48-64).

use crate::types::Handler;

/// Title handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("title", r"360.Degrees.of.Vision.The.Byakugan'?s.Blind.Spot").with_remove(),
        Handler::new("title", r"\b(?:INTERNAL|HFR)\b").with_remove(),
        Handler::new("title", r"413 Days").with_remove(),
    ]
}
