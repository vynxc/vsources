use crate::transforms::Transform;
use crate::types::Handler;

/// PPV handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("ppv", r"\bPPV(?:HD)?\b")
            .with_transform(Transform::Boolean)
            .with_remove()
            .with_skip_from_title(),
        Handler::new("ppv", r"\b\W?Fight.?Nights?\W?\b")
            .with_transform(Transform::Boolean)
            .with_skip_from_title(),
    ]
}
