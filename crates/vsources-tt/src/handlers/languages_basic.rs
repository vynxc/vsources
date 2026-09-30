//! Basic language-marker handlers (TS `handlers.ts` lines 1222-1241).

use crate::transforms::Transform;
use crate::types::Handler;

/// Basic language handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("languages", r"\b(temporadas?|completa)\b")
            .with_transform(Transform::ValueSet("es".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(?:INT[EÉ]GRALE?)\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(?:Saison)\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching(),
    ]
}
