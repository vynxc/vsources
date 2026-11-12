//! Definitive known error-video fingerprints; no URL, host or duration blacklist.
use sha2::{Digest, Sha256};
/// Prefix size shared with the lightweight binary media probe.
pub const ERROR_CLIP_PREFIX_BYTES: usize = 2048;
// Atlantic error-code4 diagnostic clip, visually verified at1080p/720p/480p.
// Hashes cover media bytes only; no private URLs, headers or videos are stored.
const ERROR_CLIP_PREFIXES: [&str; 3] = [
    "3db25a916f01ed7f0b240be3912a97c64024f542d140230b632ee9e64d1826f4",
    "72bcddc8d9af7bd6fa48e9df677b1839aa8c88bc3c267292eda1849903455e4d",
    "82ccceb10eeac3c68e8df8ec2aff765c786c1c1a1e44efbdc415fa78aeacf723",
];
/// True only for a complete, byte-identical prefix of a verified diagnostic video.
///
/// HTTP200, TS signatures, finite duration and successful decode do not establish
/// that a stream contains the requested title. Unknown/short prefixes stay unknown.
#[must_use]
pub fn known_error_clip_prefix(body: &[u8]) -> bool {
    matches_prefix(body, &ERROR_CLIP_PREFIXES)
}
fn matches_prefix(body: &[u8], known: &[&str]) -> bool {
    let Some(prefix) = body.get(..ERROR_CLIP_PREFIX_BYTES) else {
        return false;
    };
    let digest = format!("{:x}", Sha256::digest(prefix));
    known.contains(&digest.as_str())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_binary_fingerprint_not_host_duration_or_ts_headers_is_required() {
        let mut diagnostic = vec![0x47; ERROR_CLIP_PREFIX_BYTES];
        diagnostic[189] = 0x42;
        let digest = format!("{:x}", Sha256::digest(&diagnostic));
        assert!(matches_prefix(&diagnostic, &[&digest]));
        let mut with_more = diagnostic.clone();
        with_more.extend_from_slice(b"tail");
        assert!(matches_prefix(&with_more, &[&digest]));
        assert!(!matches_prefix(&diagnostic[..2047], &[&digest]));
        diagnostic[190] ^= 1;
        assert!(!matches_prefix(&diagnostic, &[&digest]));
        assert!(!known_error_clip_prefix(&diagnostic));
    }
}
