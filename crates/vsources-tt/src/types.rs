//! Core types: parsed results, parse metadata, and handler configuration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::js_regex::Regex;
use crate::processors::Processor;
use crate::transforms::Transform;
use crate::validators::Validator;

/// Fields whose values accumulate in an ordered, de-duplicated set.
pub(crate) const VALUE_SET_FIELDS: [&str; 6] = [
    "audio",
    "channels",
    "editions",
    "hdr",
    "languages",
    "releaseTypes",
];

/// Byte-span positions of a match and its groups (index 0 = whole match).
pub(crate) type MatchIndices = Vec<Option<(usize, usize)>>;

/// Per-field parse metadata during a run.
pub(crate) type ParseResult = std::collections::BTreeMap<&'static str, ParseMeta>;

/// A value produced by a handler for a field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// A string value.
    Str(String),
    /// A boolean flag.
    Bool(bool),
    /// A list of integers.
    Ints(Vec<i64>),
    /// An ordered set of unique strings.
    Set(Vec<String>),
}

/// Parse metadata tracked for one field while the handler pipeline runs.
///
/// Mirrors the TypeScript `ParseMeta` structure byte-for-byte in semantics:
/// `m_index`/`m_value` locate the match in the (mutating) working title and
/// `matched` accumulates every raw match text seen for the field.
#[derive(Debug, Clone)]
pub(crate) struct ParseMeta {
    /// Byte offset of the match inside the working title.
    pub m_index: usize,
    /// The raw matched text.
    pub m_value: String,
    /// Every raw match text recorded for this field so far.
    pub matched: Vec<String>,
    /// The field value; `None` mirrors the TypeScript `null` sentinel.
    pub value: Option<Value>,
    /// Whether the matched text should be removed from the title.
    pub remove: bool,
    /// Whether a handler already processed this field.
    pub processed: bool,
}

impl ParseMeta {
    /// A fresh meta for a field with no match yet.
    pub(crate) fn empty(value: Option<Value>) -> Self {
        Self {
            m_index: 0,
            m_value: String::new(),
            matched: Vec::new(),
            value,
            remove: false,
            processed: false,
        }
    }
}

/// Structured metadata extracted from a torrent-style release name.
///
/// Field names serialize as camelCase to match the TypeScript
/// `parse-torrent-title` wire format.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedTorrentTitle {
    /// Movie or series title.
    pub title: Option<String>,
    /// Release year, or a year range such as `1998-2003`.
    pub year: Option<String>,
    /// Release date, normalized to `YYYY-MM-DD`.
    pub date: Option<String>,
    /// Country variant of a release, e.g. `US`, `UK`.
    pub country: Option<String>,
    /// Vertical resolution label, e.g. `1080p`, `2160p`.
    pub resolution: Option<String>,
    /// Source quality, e.g. `BluRay`, `WEB-DL`.
    pub quality: Option<String>,
    /// Video codec, e.g. `x264`, `HEVC`.
    pub codec: Option<String>,
    /// Color bit depth, e.g. `10bit`.
    pub bit_depth: Option<String>,
    /// HDR variants, e.g. `HDR10`, `Dolby Vision`.
    pub hdr: Option<Vec<String>>,
    /// Stereo format, e.g. `3D SBS`.
    #[serde(rename = "threeD")]
    pub three_d: Option<String>,
    /// Audio codecs, e.g. `DTS`, `AAC`.
    pub audio: Option<Vec<String>>,
    /// Audio channel layouts, e.g. `5.1`.
    pub channels: Option<Vec<String>>,
    /// Season numbers.
    pub seasons: Option<Vec<i64>>,
    /// Episode numbers.
    pub episodes: Option<Vec<i64>>,
    /// Production episode code, e.g. `5E46AC39`.
    pub episode_code: Option<String>,
    /// Episode title following the episode marker.
    pub episode_title: Option<String>,
    /// Complete season/series indicator.
    pub complete: Option<bool>,
    /// Volume numbers.
    pub volumes: Option<Vec<i64>>,
    /// Language tags, e.g. `en`, `jp`, `multi subs`.
    pub languages: Option<Vec<String>>,
    /// Whether content is dubbed.
    pub dubbed: Option<bool>,
    /// Whether subtitles are included.
    pub subbed: Option<bool>,
    /// Whether subtitles are hardcoded.
    pub hardcoded: Option<bool>,
    /// Release group name.
    pub group: Option<String>,
    /// Source site.
    pub site: Option<String>,
    /// Broadcasting network, e.g. `Netflix`, `HBO`.
    pub network: Option<String>,
    /// Editions, e.g. `Extended Edition`, `IMAX`.
    pub editions: Option<Vec<String>>,
    /// Release types, e.g. `OVA`, `OAD`.
    pub release_types: Option<Vec<String>>,
    /// Repack indicator.
    pub repack: Option<bool>,
    /// Proper release indicator.
    pub proper: Option<bool>,
    /// Retail release indicator.
    pub retail: Option<bool>,
    /// Colour regraded video indicator.
    pub regraded: Option<bool>,
    /// Unrated version indicator.
    pub unrated: Option<bool>,
    /// Uncensored version indicator.
    pub uncensored: Option<bool>,
    /// Extended version indicator.
    pub extended: Option<bool>,
    /// Converted release indicator.
    pub convert: Option<bool>,
    /// Documentary indicator.
    pub documentary: Option<bool>,
    /// Commentary track indicator.
    pub commentary: Option<bool>,
    /// Upscaled content indicator.
    pub upscaled: Option<bool>,
    /// File container, e.g. `mkv`.
    pub container: Option<String>,
    /// File extension, e.g. `mkv`.
    pub extension: Option<String>,
    /// Regional encoding, e.g. `R1`.
    pub region: Option<String>,
    /// File size, e.g. `1.4 GB`.
    pub size: Option<String>,
    /// Pay-per-view indicator.
    pub ppv: Option<bool>,
    /// Values produced by custom handlers for non-standard fields.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, ExtraValue>,
}

/// Values that custom handlers may publish for non-standard fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtraValue {
    /// A single string.
    Str(String),
    /// A boolean flag.
    Bool(bool),
    /// A list of integers.
    Ints(Vec<i64>),
    /// A list of strings.
    Strs(Vec<String>),
}

/// A single rule in the parsing pipeline.
///
/// Handlers are tried in order against the working title; the first match
/// wins for a field unless `keep_matching` is set.
#[derive(Debug, Clone)]
// The boolean options mirror the upstream handler configuration one-to-one;
// folding them into a bitflags struct would break that correspondence.
#[allow(clippy::struct_excessive_bools)]
pub struct Handler {
    /// Target field name (e.g. `"resolution"`).
    pub(crate) field: &'static str,
    /// The pattern tested against the title.
    pub(crate) pattern: Option<Regex>,
    /// Optional extra validation over the match.
    pub(crate) validator: Option<Validator>,
    /// Optional transformation of the captured value.
    pub(crate) transform: Option<Transform>,
    /// Optional processor run after pattern matching.
    pub(crate) processor: Option<Processor>,
    /// Whether the matched text is removed from the title.
    pub(crate) remove: bool,
    /// `!skipIfAlreadyFound` — keep matching after the field is set.
    pub(crate) keep_matching: bool,
    /// Skip when other fields matched earlier in the title.
    pub(crate) skip_if_first: bool,
    /// Skip when any of these fields matched earlier.
    pub(crate) skip_if_before: &'static [&'static str],
    /// Never let this match shorten the title.
    pub(crate) skip_from_title: bool,
    /// Re-scan past the title boundary for a later occurrence.
    pub(crate) retry_past_title: Option<Regex>,
    /// Only match when the token is the last word of the title.
    pub(crate) must_end_title: bool,
    /// Group index used for the match position.
    pub(crate) match_group: Option<usize>,
    /// Group index used for the field value (defaults to 1).
    pub(crate) value_group: Option<usize>,
}

impl Handler {
    /// Start a handler with a case-insensitive JavaScript-style pattern.
    #[must_use]
    pub fn new(field: &'static str, pattern: &str) -> Self {
        Self {
            field,
            pattern: crate::js_regex::compile_ci(pattern),
            validator: None,
            transform: None,
            processor: None,
            remove: false,
            keep_matching: false,
            skip_if_first: false,
            skip_if_before: &[],
            skip_from_title: false,
            retry_past_title: None,
            must_end_title: false,
            match_group: None,
            value_group: None,
        }
    }

    /// Start a handler with a case-sensitive pattern.
    #[must_use]
    pub fn new_case_sensitive(field: &'static str, pattern: &str) -> Self {
        let mut h = Self::new(field, pattern);
        h.pattern = crate::js_regex::compile(pattern, false);
        h
    }

    /// Start a pattern-less handler (process-only).
    #[must_use]
    pub fn process_only(field: &'static str, processor: Processor) -> Self {
        Self {
            field,
            pattern: None,
            validator: None,
            transform: None,
            processor: Some(processor),
            remove: false,
            keep_matching: false,
            skip_if_first: false,
            skip_if_before: &[],
            skip_from_title: false,
            retry_past_title: None,
            must_end_title: false,
            match_group: None,
            value_group: None,
        }
    }

    /// Remove the matched text from the working title.
    #[must_use]
    pub fn with_remove(mut self) -> Self {
        self.remove = true;
        self
    }

    /// Keep matching after the field already has a value.
    #[must_use]
    pub fn with_keep_matching(mut self) -> Self {
        self.keep_matching = true;
        self
    }

    /// Skip when other fields matched earlier in the title.
    #[must_use]
    pub fn with_skip_if_first(mut self) -> Self {
        self.skip_if_first = true;
        self
    }

    /// Skip when any of these fields matched earlier.
    #[must_use]
    pub fn with_skip_if_before(mut self, fields: &'static [&'static str]) -> Self {
        self.skip_if_before = fields;
        self
    }

    /// Never let this match shorten the title.
    #[must_use]
    pub fn with_skip_from_title(mut self) -> Self {
        self.skip_from_title = true;
        self
    }

    /// Re-scan past the title boundary for a later occurrence.
    #[must_use]
    pub fn with_retry_past_title(mut self, pattern: &str) -> Self {
        self.retry_past_title = crate::js_regex::compile_ci(pattern);
        self
    }

    /// Only match when the token is the last word of the title.
    #[must_use]
    pub fn with_must_end_title(mut self) -> Self {
        self.must_end_title = true;
        self
    }

    /// Use this group index for the match position.
    #[must_use]
    pub fn with_match_group(mut self, group: usize) -> Self {
        self.match_group = Some(group);
        self
    }

    /// Use this group index for the field value.
    #[must_use]
    pub fn with_value_group(mut self, group: usize) -> Self {
        self.value_group = Some(group);
        self
    }

    /// Attach a value transformation.
    #[must_use]
    pub fn with_transform(mut self, transform: Transform) -> Self {
        self.transform = Some(transform);
        self
    }

    /// Attach a match validator.
    #[must_use]
    pub fn with_validator(mut self, validator: Validator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Attach a processor.
    #[must_use]
    pub fn with_processor(mut self, processor: Processor) -> Self {
        self.processor = Some(processor);
        self
    }
}
