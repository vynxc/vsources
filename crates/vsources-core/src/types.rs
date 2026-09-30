//! Core value types: media references, streams, and provider metadata.
//!
//! Ports the TypeScript `src/types.js` plus the result shapes used by the
//! source/extractor pipeline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::Url;

/// Content kind a provider can serve.
pub type ContentTypes = Vec<MediaType>;

/// The media kinds providers distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaType {
    /// A feature film.
    Movie,
    /// An episodic series.
    Series,
}

/// Stream container format.
///
/// Ports `Format` from the TypeScript `types.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// An HLS playlist (`.m3u8`).
    Hls,
    /// A progressive video file (`.mp4`, `.mkv`, …).
    Mp4,
    /// Unknown/unresolvable.
    Unknown,
}

/// Language/region tags attached to streams.
///
/// Ports `CountryCode` from the TypeScript `types.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CountryCode {
    /// Multiple or unspecified languages.
    Multi,
    /// English.
    En,
    /// Hindi.
    Hi,
    /// Tamil.
    Ta,
    /// Telugu.
    Te,
    /// Gujarati.
    Gu,
    /// Malayalam.
    Ml,
    /// Punjabi.
    Pa,
    /// Marathi.
    Mr,
    /// Kannada.
    Kn,
    /// German.
    De,
    /// French.
    Fr,
    /// Castilian Spanish.
    Es,
    /// Latin American Spanish.
    Mx,
    /// Italian.
    It,
    /// Portuguese.
    Pt,
    /// Japanese.
    Ja,
    /// Korean.
    Ko,
    /// Chinese.
    Zh,
    /// Arabic.
    Ar,
    /// Turkish.
    Tr,
    /// Russian.
    Ru,
    /// Polish.
    Pl,
    /// Dutch.
    Nl,
    /// Romanian.
    Ro,
    /// Bulgarian.
    Bg,
    /// Croatian.
    Hr,
    /// Czech.
    Cs,
    /// Greek.
    El,
    /// Hebrew.
    He,
    /// Hungarian.
    Hu,
    /// Slovak.
    Sk,
    /// Slovenian.
    Sl,
    /// Serbian.
    Sr,
    /// Ukrainian.
    Uk,
    /// Vietnamese.
    Vi,
    /// Thai.
    Th,
    /// Indonesian.
    Id,
    /// Estonian.
    Et,
    /// Lithuanian.
    Lt,
    /// Latvian.
    Lv,
    /// Norwegian.
    No,
    /// Persian.
    Fa,
    /// Albanian.
    Bl,
    /// Bengali.
    Al,
}

impl CountryCode {
    /// Parse a tag as used across the `PhoeniX` codebase.
    #[must_use]
    pub fn parse(tag: &str) -> Option<Self> {
        Some(match tag {
            "multi" => Self::Multi,
            "en" => Self::En,
            "hi" => Self::Hi,
            "ta" => Self::Ta,
            "te" => Self::Te,
            "gu" => Self::Gu,
            "ml" => Self::Ml,
            "pa" => Self::Pa,
            "mr" => Self::Mr,
            "kn" => Self::Kn,
            "de" => Self::De,
            "fr" => Self::Fr,
            "es" => Self::Es,
            "mx" => Self::Mx,
            "it" => Self::It,
            "pt" => Self::Pt,
            "ja" => Self::Ja,
            "ko" => Self::Ko,
            "zh" => Self::Zh,
            "ar" => Self::Ar,
            "tr" => Self::Tr,
            "ru" => Self::Ru,
            "pl" => Self::Pl,
            "nl" => Self::Nl,
            "ro" => Self::Ro,
            "bg" => Self::Bg,
            "hr" => Self::Hr,
            "cs" => Self::Cs,
            "el" => Self::El,
            "he" => Self::He,
            "hu" => Self::Hu,
            "sk" => Self::Sk,
            "sl" => Self::Sl,
            "sr" => Self::Sr,
            "uk" => Self::Uk,
            "vi" => Self::Vi,
            "th" => Self::Th,
            "id" => Self::Id,
            "et" => Self::Et,
            "lt" => Self::Lt,
            "lv" => Self::Lv,
            "no" => Self::No,
            "fa" => Self::Fa,
            "bl" => Self::Bl,
            "al" => Self::Al,
            _ => return None,
        })
    }

    /// The tag string used across the `PhoeniX` codebase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Multi => "multi",
            Self::En => "en",
            Self::Hi => "hi",
            Self::Ta => "ta",
            Self::Te => "te",
            Self::Gu => "gu",
            Self::Ml => "ml",
            Self::Pa => "pa",
            Self::Mr => "mr",
            Self::Kn => "kn",
            Self::De => "de",
            Self::Fr => "fr",
            Self::Es => "es",
            Self::Mx => "mx",
            Self::It => "it",
            Self::Pt => "pt",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::Zh => "zh",
            Self::Ar => "ar",
            Self::Tr => "tr",
            Self::Ru => "ru",
            Self::Pl => "pl",
            Self::Nl => "nl",
            Self::Ro => "ro",
            Self::Bg => "bg",
            Self::Hr => "hr",
            Self::Cs => "cs",
            Self::El => "el",
            Self::He => "he",
            Self::Hu => "hu",
            Self::Sk => "sk",
            Self::Sl => "sl",
            Self::Sr => "sr",
            Self::Uk => "uk",
            Self::Vi => "vi",
            Self::Th => "th",
            Self::Id => "id",
            Self::Et => "et",
            Self::Lt => "lt",
            Self::Lv => "lv",
            Self::No => "no",
            Self::Fa => "fa",
            Self::Bl => "bl",
            Self::Al => "al",
        }
    }
}

/// A media identifier, either `TMDB` or `IMDb`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MediaId {
    /// A numeric TMDB id.
    Tmdb(u64),
    /// An `IMDb` id string such as `tt0944947`.
    Imdb(String),
}

impl MediaId {
    /// Build a TMDB id.
    pub fn tmdb(id: u64) -> Self {
        Self::Tmdb(id)
    }

    /// Build an `IMDb` id (adds the `tt` prefix when missing).
    pub fn imdb(id: impl Into<String>) -> Self {
        let s = id.into();
        if s.starts_with("tt") {
            Self::Imdb(s)
        } else {
            Self::Imdb(format!("tt{s}"))
        }
    }

    /// Parse a string like `tmdb:1396`, `1396`, or `tt0944947`.
    ///
    /// The `tmdb:` prefix is optional; an `IMDb` id is a `tt`-prefixed
    /// digit run. Anything else (including stray season/episode parts —
    /// use the [`MediaRef`] constructors for
    /// those) answers `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        let head = parts.next().unwrap_or_default();
        if head.eq_ignore_ascii_case("tmdb") {
            let digits = parts.next().unwrap_or_default();
            // Trailing parts (season/episode) are not consumed here —
            // use the `MediaRef` constructors or `parse_media_ref`.
            let valid = !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit())
                && parts.next().is_none();
            return valid
                .then(|| digits.parse::<u64>().ok())
                .flatten()
                .map(Self::Tmdb);
        }
        if head.starts_with("tt") && head.len() > 2 && head[2..].chars().all(|c| c.is_ascii_digit())
        {
            Some(Self::Imdb(head.to_string()))
        } else if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            head.parse::<u64>().ok().map(Self::Tmdb)
        } else {
            None
        }
    }

    /// The numeric TMDB id, when this is one.
    #[must_use]
    pub fn as_tmdb(&self) -> Option<u64> {
        match self {
            Self::Tmdb(id) => Some(*id),
            Self::Imdb(_) => None,
        }
    }

    /// The `IMDb` id string, when this is one.
    #[must_use]
    pub fn as_imdb(&self) -> Option<&str> {
        match self {
            Self::Imdb(s) => Some(s),
            Self::Tmdb(_) => None,
        }
    }
}

/// What to resolve: a movie, or an episode of a series.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaRef {
    /// The external id.
    pub id: MediaId,
    /// Movie or series.
    pub kind: MediaType,
    /// Season for series references.
    pub season: Option<u32>,
    /// Episode for series references.
    pub episode: Option<u32>,
}

impl MediaRef {
    /// A movie reference.
    pub fn movie(id: MediaId) -> Self {
        Self {
            id,
            kind: MediaType::Movie,
            season: None,
            episode: None,
        }
    }

    /// A series reference.
    pub fn series(id: MediaId, season: u32, episode: u32) -> Self {
        Self {
            id,
            kind: MediaType::Series,
            season: Some(season),
            episode: Some(episode),
        }
    }

    /// A TMDB-keyed reference.
    pub fn tmdb(id: u64, kind: MediaType) -> Self {
        Self {
            id: MediaId::Tmdb(id),
            kind,
            season: None,
            episode: None,
        }
    }

    /// An IMDb-keyed reference.
    pub fn imdb(id: impl Into<String>, kind: MediaType) -> Self {
        Self {
            id: MediaId::imdb(id),
            kind,
            season: None,
            episode: None,
        }
    }

    /// `S01E05` when this is an episode reference.
    #[must_use]
    pub fn format_season_and_episode(&self) -> String {
        format!(
            "S{:02}E{:02}",
            self.season.unwrap_or(1),
            self.episode.unwrap_or(1)
        )
    }
}

/// A subtitle track attached to a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// Track label (e.g. `English`).
    pub label: Option<String>,
    /// Language tag (ISO 639 or name).
    pub language: Option<String>,
    /// Subtitle file/playlist URL.
    pub url: Url,
}

/// Metadata describing a resolved stream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamMeta {
    /// Source quality label (e.g. `BluRay`, `WEB-DL`).
    pub quality: Option<String>,
    /// Vertical resolution in pixels (e.g. 1080).
    pub resolution: Option<u16>,
    /// Video codec family (e.g. `HEVC`, `AVC`, `AV1`).
    pub codec: Option<String>,
    /// Audio codec labels.
    pub audio: Vec<String>,
    /// Language/region tags.
    pub languages: Vec<CountryCode>,
    /// Whether the audio track is dubbed (vs. the original language).
    pub dubbed: Option<bool>,
    /// Whether the stream is subtitled (a `sub`/`SUB` marker in the
    /// release name or label).
    pub subbed: Option<bool>,
    /// File size in bytes, when known.
    pub size: Option<u64>,
    /// Human-readable size (e.g. `1.4 GB`), when the byte size is unknown.
    pub size_label: Option<String>,
    /// The providing source's id.
    pub source_id: Option<String>,
    /// The providing source's display label.
    pub source_label: Option<String>,
    /// The extractor that resolved the stream, when applicable.
    pub extractor_label: Option<String>,
    /// Headers a player must send with the stream URL (Referer, UA).
    pub request_headers: BTreeMap<String, String>,
    /// Subtitle tracks offered alongside the stream.
    pub subtitles: Vec<SubtitleTrack>,
    /// Video bitrate hint (bps), when known.
    pub bitrate: Option<u64>,
}

impl StreamMeta {
    /// Insert a request header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.request_headers.insert(name.into(), value.into());
        self
    }
}

/// One resolved, playable stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    /// The direct (or embed) URL.
    pub url: Url,
    /// Container format.
    pub format: Format,
    /// Display label for the stream.
    pub label: Option<String>,
    /// Parsed metadata.
    pub meta: StreamMeta,
    /// How long a resolver may cache this result.
    pub ttl: std::time::Duration,
    /// Whether the URL is an external page rather than a direct stream.
    pub is_external: bool,
    /// Behaviour hints (Stremio-style) for clients.
    pub behavior_hints: BTreeMap<String, String>,
}

impl Stream {
    /// Build a stream of unknown format.
    pub fn new(url: Url, format: Format) -> Self {
        Self {
            url,
            format,
            label: None,
            meta: StreamMeta::default(),
            ttl: std::time::Duration::from_secs(300),
            is_external: false,
            behavior_hints: BTreeMap::new(),
        }
    }

    /// Attach a display label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Override the cache lifetime.
    #[must_use]
    pub fn with_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Mark the URL as an external page rather than a direct stream.
    #[must_use]
    pub fn mark_external(mut self) -> Self {
        self.is_external = true;
        self
    }

    /// Require a `Referer` header on stream requests.
    #[must_use]
    pub fn with_referer(mut self, referer: impl Into<String>) -> Self {
        self.meta = self.meta.with_header("Referer", referer);
        self
    }
}

/// Descriptive metadata for a provider, exposed via listings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Stable provider id (e.g. `allwish`).
    pub id: String,
    /// Display label (e.g. `AllWish`).
    pub label: String,
    /// Content kinds this provider serves.
    pub content_types: ContentTypes,
    /// Language tags this provider can produce.
    pub country_codes: Vec<CountryCode>,
    /// The provider's canonical base URL, when fixed.
    pub base_url: Option<Url>,
    /// Sort priority (higher first).
    pub priority: i32,
    /// Domain key for `{KEY}_BASE_URL` env overrides.
    pub domain_key: Option<String>,
}
