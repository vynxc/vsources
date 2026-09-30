//! Resolution helpers.
//!
//! Ports `src/utils/resolution.js` and `src/utils/height.js`.

use crate::traits::Fetcher;

/// Known vertical resolutions, highest first.
pub const RESOLUTIONS: &[u16] = &[2160, 1440, 1080, 720, 576, 480, 360, 240, 144];

/// The closest known resolution label for a height.
#[must_use]
pub fn closest_resolution(height: u16) -> String {
    let mut closest = RESOLUTIONS[0];
    for candidate in RESOLUTIONS {
        if candidate.abs_diff(height) < closest.abs_diff(height) {
            closest = *candidate;
        }
    }
    format!("{closest}p")
}

/// Find the height implied by a label like `1080p` inside `value`.
#[must_use]
pub fn find_height(value: &str) -> Option<u16> {
    let lower = value.to_lowercase();
    RESOLUTIONS
        .iter()
        .find(|r| lower.contains(&format!("{r}p")))
        .copied()
}

/// Guess the best height from an HLS playlist's `WxH` / `NNNp` lines.
///
/// Ports `guessHeightFromPlaylist`.
pub async fn guess_height_from_playlist(fetcher: &dyn Fetcher, url: &url::Url) -> Option<u16> {
    let body = crate::traits::fetch_text(fetcher, url.clone()).await.ok()?;
    let mut best: Option<u16> = None;
    for line in body.lines() {
        for token in line.split([',', ' ', '"']) {
            let token = token.trim();
            let height = token
                .strip_suffix('p')
                .and_then(|digits| digits.parse::<u16>().ok())
                .or_else(|| {
                    // WxH form: take the part after 'x'.
                    token
                        .rsplit_once('x')
                        .and_then(|(_, h)| h.parse::<u16>().ok())
                });
            if let Some(height) = height {
                best = Some(best.map_or(height, |b| b.max(height)));
            }
        }
    }
    best
}
