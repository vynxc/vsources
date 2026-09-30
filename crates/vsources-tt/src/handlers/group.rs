//! Final group handlers (TS `handlers.ts` lines 3414-3435).

use crate::processors::Processor;
use crate::types::Handler;

/// Handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("group", r"\b(\w+-raws)(?:\.com)?\b").with_remove(),
        Handler::process_only("group", Processor::Group),
    ]
}
