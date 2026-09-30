//! Volumes handlers (TS `handlers.ts` lines 1190-1221, "Batch 6").

use crate::processors::Processor;
use crate::transforms::Transform;
use crate::types::Handler;

/// Volumes handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "volumes",
            r"\bvol(?:s|umes?)?[. -]*(?:\d{1,3}[., +/\\&-]+)+\d{1,3}\b",
        )
        .with_transform(Transform::IntRange)
        .with_remove(),
        // Process-only handler: `vol(ume) N` detection that only starts
        // looking after a matched year.
        Handler::process_only("volumes", Processor::Volumes),
    ]
}
