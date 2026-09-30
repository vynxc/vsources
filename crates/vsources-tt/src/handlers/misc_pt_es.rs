//! Portuguese/Spanish episode language fallback
//! (TS `handlers.ts` lines 3009-3037).

use crate::processors::Processor;
use crate::types::Handler;

/// Handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![Handler::process_only(
        "languages",
        Processor::PortugueseLanguages,
    )]
}
