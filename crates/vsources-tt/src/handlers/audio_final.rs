//! Final MP3 audio handler (TS `handlers.ts` lines 3444-3452).

use crate::transforms::Transform;
use crate::types::Handler;

/// Handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("audio", r"\bMP3\b")
            .with_transform(Transform::ValueSet("MP3".to_string()))
            .with_remove()
            .with_keep_matching(),
    ]
}
