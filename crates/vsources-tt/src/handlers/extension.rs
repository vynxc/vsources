//! Extension handler (TS `handlers.ts` lines 3436-3443).

use crate::transforms::Transform;
use crate::types::Handler;

/// Handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![Handler::new(
        "extension",
        r"\.(3g2|3gp|avi|flv|mkv|mk3d|mov|mp2|mp4|m4v|mpe|mpeg|mpg|mpv|webm|wmv|ogm|divx|ts|m2ts|iso|vob|sub|idx|ttxt|txt|smi|srt|ssa|ass|vtt|nfo|html)$",
    )
    .with_transform(Transform::Lowercase)]
}
