//! Language utilities: tags, flags, and detection from text.
//!
//! Ports `src/utils/language.js`.

use crate::types::CountryCode;

/// Static per-tag language info (name, flag, ISO 639-2/B code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInfo {
    /// Tag key.
    pub code: CountryCode,
    /// Human language name.
    pub language: &'static str,
    /// Flag emoji.
    pub flag: &'static str,
    /// ISO 639-2/B bibliographic code, when assigned.
    pub iso639: Option<&'static str>,
}

/// The full tag table, mirroring `language.js`'s `countryCodeMap`.
pub const LANGUAGE_TABLE: &[LanguageInfo] = &[
    LanguageInfo {
        code: CountryCode::Multi,
        language: "Multi",
        flag: "🌐",
        iso639: None,
    },
    LanguageInfo {
        code: CountryCode::Al,
        language: "Albanian",
        flag: "🇦🇱",
        iso639: Some("alb"),
    },
    LanguageInfo {
        code: CountryCode::Ar,
        language: "Arabic",
        flag: "🇸🇦",
        iso639: Some("ara"),
    },
    LanguageInfo {
        code: CountryCode::Bg,
        language: "Bulgarian",
        flag: "🇧🇬",
        iso639: Some("bul"),
    },
    LanguageInfo {
        code: CountryCode::Bl,
        language: "Bengali",
        flag: "🇮🇳",
        iso639: Some("ben"),
    },
    LanguageInfo {
        code: CountryCode::Cs,
        language: "Czech",
        flag: "🇨🇿",
        iso639: Some("ces"),
    },
    LanguageInfo {
        code: CountryCode::De,
        language: "German",
        flag: "🇩🇪",
        iso639: Some("ger"),
    },
    LanguageInfo {
        code: CountryCode::El,
        language: "Greek",
        flag: "🇬🇷",
        iso639: Some("gre"),
    },
    LanguageInfo {
        code: CountryCode::En,
        language: "English",
        flag: "🇺🇸",
        iso639: Some("eng"),
    },
    LanguageInfo {
        code: CountryCode::Es,
        language: "Castilian Spanish",
        flag: "🇪🇸",
        iso639: Some("spa"),
    },
    LanguageInfo {
        code: CountryCode::Et,
        language: "Estonian",
        flag: "🇪🇪",
        iso639: Some("est"),
    },
    LanguageInfo {
        code: CountryCode::Fa,
        language: "Persian",
        flag: "🇮🇷",
        iso639: Some("fas"),
    },
    LanguageInfo {
        code: CountryCode::Fr,
        language: "French",
        flag: "🇫🇷",
        iso639: Some("fra"),
    },
    LanguageInfo {
        code: CountryCode::Gu,
        language: "Gujarati",
        flag: "🇮🇳",
        iso639: Some("guj"),
    },
    LanguageInfo {
        code: CountryCode::He,
        language: "Hebrew",
        flag: "🇮🇱",
        iso639: Some("heb"),
    },
    LanguageInfo {
        code: CountryCode::Hi,
        language: "Hindi",
        flag: "🇮🇳",
        iso639: Some("hin"),
    },
    LanguageInfo {
        code: CountryCode::Hr,
        language: "Croatian",
        flag: "🇭🇷",
        iso639: Some("hrv"),
    },
    LanguageInfo {
        code: CountryCode::Hu,
        language: "Hungarian",
        flag: "🇭🇺",
        iso639: Some("hun"),
    },
    LanguageInfo {
        code: CountryCode::Id,
        language: "Indonesian",
        flag: "🇮🇩",
        iso639: Some("ind"),
    },
    LanguageInfo {
        code: CountryCode::It,
        language: "Italian",
        flag: "🇮🇹",
        iso639: Some("ita"),
    },
    LanguageInfo {
        code: CountryCode::Ja,
        language: "Japanese",
        flag: "🇯🇵",
        iso639: Some("jpn"),
    },
    LanguageInfo {
        code: CountryCode::Kn,
        language: "Kannada",
        flag: "🇮🇳",
        iso639: Some("kan"),
    },
    LanguageInfo {
        code: CountryCode::Ko,
        language: "Korean",
        flag: "🇰🇷",
        iso639: Some("kor"),
    },
    LanguageInfo {
        code: CountryCode::Lt,
        language: "Lithuanian",
        flag: "🇱🇹",
        iso639: Some("lit"),
    },
    LanguageInfo {
        code: CountryCode::Lv,
        language: "Latvian",
        flag: "🇱🇻",
        iso639: Some("lav"),
    },
    LanguageInfo {
        code: CountryCode::Ml,
        language: "Malayalam",
        flag: "🇮🇳",
        iso639: Some("mal"),
    },
    LanguageInfo {
        code: CountryCode::Mr,
        language: "Marathi",
        flag: "🇮🇳",
        iso639: Some("mar"),
    },
    LanguageInfo {
        code: CountryCode::Mx,
        language: "Latin American Spanish",
        flag: "🇲🇽",
        iso639: Some("spa"),
    },
    LanguageInfo {
        code: CountryCode::Nl,
        language: "Dutch",
        flag: "🇳🇱",
        iso639: Some("nld"),
    },
    LanguageInfo {
        code: CountryCode::No,
        language: "Norwegian",
        flag: "🇳🇴",
        iso639: Some("nor"),
    },
    LanguageInfo {
        code: CountryCode::Pa,
        language: "Punjabi",
        flag: "🇮🇳",
        iso639: Some("pan"),
    },
    LanguageInfo {
        code: CountryCode::Pl,
        language: "Polish",
        flag: "🇵🇱",
        iso639: Some("pol"),
    },
    LanguageInfo {
        code: CountryCode::Pt,
        language: "Portuguese",
        flag: "🇧🇷",
        iso639: Some("por"),
    },
    LanguageInfo {
        code: CountryCode::Ro,
        language: "Romanian",
        flag: "🇷🇴",
        iso639: Some("ron"),
    },
    LanguageInfo {
        code: CountryCode::Ru,
        language: "Russian",
        flag: "🇷🇺",
        iso639: Some("rus"),
    },
    LanguageInfo {
        code: CountryCode::Sk,
        language: "Slovak",
        flag: "🇸🇰",
        iso639: Some("slk"),
    },
    LanguageInfo {
        code: CountryCode::Sl,
        language: "Slovenian",
        flag: "🇸🇮",
        iso639: Some("slv"),
    },
    LanguageInfo {
        code: CountryCode::Sr,
        language: "Serbian",
        flag: "🇷🇸",
        iso639: Some("srp"),
    },
    LanguageInfo {
        code: CountryCode::Ta,
        language: "Tamil",
        flag: "🇮🇳",
        iso639: Some("tam"),
    },
    LanguageInfo {
        code: CountryCode::Te,
        language: "Telugu",
        flag: "🇮🇳",
        iso639: Some("tel"),
    },
    LanguageInfo {
        code: CountryCode::Th,
        language: "Thai",
        flag: "🇹🇭",
        iso639: Some("tha"),
    },
    LanguageInfo {
        code: CountryCode::Tr,
        language: "Turkish",
        flag: "🇹🇷",
        iso639: Some("tur"),
    },
    LanguageInfo {
        code: CountryCode::Uk,
        language: "Ukrainian",
        flag: "🇺🇦",
        iso639: Some("ukr"),
    },
    LanguageInfo {
        code: CountryCode::Vi,
        language: "Vietnamese",
        flag: "🇻🇳",
        iso639: Some("vie"),
    },
    LanguageInfo {
        code: CountryCode::Zh,
        language: "Chinese",
        flag: "🇨🇳",
        iso639: Some("zho"),
    },
];

/// The flag emoji for a tag.
#[must_use]
pub fn flag_from_country_code(code: CountryCode) -> &'static str {
    LANGUAGE_TABLE
        .iter()
        .find(|e| e.code == code)
        .map_or("", |e| e.flag)
}

/// The language name for a tag.
#[must_use]
pub fn language_from_country_code(code: CountryCode) -> &'static str {
    LANGUAGE_TABLE
        .iter()
        .find(|e| e.code == code)
        .map_or_else(|| code.as_str(), |e| e.language)
}

/// The ISO 639-2/B code for a tag.
#[must_use]
pub fn iso639_from_country_code(code: CountryCode) -> Option<&'static str> {
    LANGUAGE_TABLE
        .iter()
        .find(|e| e.code == code)
        .and_then(|e| e.iso639)
}

/// Detect language tags whose names appear in `value`.
///
/// Ports `findCountryCodes`: used on stream labels and HTML snippets to
/// derive per-stream language tags.
#[must_use]
pub fn find_country_codes(value: &str) -> Vec<CountryCode> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    for entry in LANGUAGE_TABLE {
        if result.contains(&entry.code) {
            continue;
        }
        if value.contains(entry.language) {
            result.push(entry.code);
        }
    }
    result
}
