#![deny(missing_docs)]
//! Port of [parse-torrent-title](https://github.com/Viren070/parse-torrent-title)
//! (TypeScript) to pure Rust.
//!
//! Extracts structured metadata — title, year, resolution, quality, codec,
//! audio, channels, seasons, episodes, languages, release group, editions
//! and much more — from torrent-style release names such as
//! `The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1`.
//!
//! This crate performs no I/O and has no async runtime dependency, so it
//! can be embedded anywhere, including on mobile clients.
//!
//! ```
//! use vsources_tt::parse_torrent_title;
//!
//! let parsed = parse_torrent_title("The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1");
//! assert_eq!(parsed.title.as_deref(), Some("The Matrix"));
//! assert_eq!(parsed.year.as_deref(), Some("1999"));
//! assert_eq!(parsed.resolution.as_deref(), Some("1080p"));
//! assert_eq!(parsed.quality.as_deref(), Some("BluRay"));
//! assert_eq!(parsed.codec.as_deref(), Some("x264"));
//! ```

mod handlers;
mod js_regex;
mod parser;
mod processors;
mod transforms;
mod types;
mod utils;
mod validators;

pub use parser::parse_torrent_title;
pub use transforms::Transform;
pub use types::{ExtraValue, Handler, ParsedTorrentTitle};
pub use validators::Validator;

/// Log a pattern that failed to compile (skipped handler) at debug level.
#[allow(clippy::print_stderr)]
pub(crate) fn log_invalid_pattern(pattern: &str, err: &fancy_regex::Error) {
    // No logging dependency in this crate by design; a compile failure in
    // the static handler table is a programmer error surfaced by the
    // handler-count test. Emit to stderr so it is not silent.
    eprintln!("vsources-tt: skipping handler with invalid pattern {pattern:?}: {err}");
}
