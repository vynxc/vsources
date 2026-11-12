use vsources_core::types::CountryCode;

#[derive(Debug, Clone, Default)]
pub(super) struct TechnicalHints {
    pub codec: Option<String>,
    pub audio: Vec<String>,
}

pub(super) fn parse_height(quality: &str) -> Option<u16> {
    let quality = quality.split('|').next()?.trim().to_ascii_lowercase();
    if quality == "4k" {
        return Some(2160);
    }
    quality
        .strip_suffix('p')
        .unwrap_or(&quality)
        .parse()
        .ok()
        .filter(|height| *height > 0)
}

pub(super) fn parse_language(language: &str) -> String {
    let value = language.trim();
    if parse_country_codes(value).is_empty() {
        String::new()
    } else {
        value.to_owned()
    }
}

pub(super) fn parse_country_codes(language: &str) -> Vec<CountryCode> {
    let lower = language.to_ascii_lowercase();
    // Subtitle declarations do not establish spoken audio.
    if lower
        .split_whitespace()
        .any(|word| matches!(word, "sub" | "subs" | "subbed" | "subtitle" | "subtitles"))
    {
        return Vec::new();
    }
    let mut codes = Vec::new();
    for part in lower.split(|c: char| c.is_whitespace() || matches!(c, ',' | '/' | '+' | '&')) {
        let code = match part {
            "english" | "eng" => Some(CountryCode::En),
            "hindi" | "hin" => Some(CountryCode::Hi),
            "japanese" | "jpn" => Some(CountryCode::Ja),
            "tamil" => Some(CountryCode::Ta),
            "telugu" => Some(CountryCode::Te),
            "kannada" => Some(CountryCode::Kn),
            "french" => Some(CountryCode::Fr),
            "spanish" => Some(CountryCode::Es),
            "german" => Some(CountryCode::De),
            "" | "audio" | "dub" | "dubbed" | "original" => None,
            _ => {
                let code = CountryCode::parse(part);
                if code.is_none() {
                    return Vec::new();
                }
                code
            }
        };
        if let Some(code) = code
            && !codes.contains(&code)
        {
            codes.push(code);
        }
    }
    codes
}

pub(super) fn language_from_label(label: &str) -> String {
    label
        .split_once('|')
        .map_or_else(String::new, |(_, language)| language.trim().to_owned())
}

fn quoted_attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (_, attributes) = line.split_once(':')?;
    let mut quoted = false;
    let mut start = 0;
    for (index, ch) in attributes
        .char_indices()
        .chain(std::iter::once((attributes.len(), ',')))
    {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            let field = attributes[start..index].trim();
            if let Some((key, value)) = field.split_once('=')
                && key == name
            {
                return value.strip_prefix('"')?.strip_suffix('"');
            }
            start = index + 1;
        }
    }
    None
}

pub(super) fn manifest_hints(line: &str) -> TechnicalHints {
    let mut hints = TechnicalHints::default();
    let mut conflicting_video = false;
    for codec in quoted_attribute(line, "CODECS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
    {
        let video = match codec.split('.').next().unwrap_or_default() {
            "avc1" | "avc3" => Some("AVC"),
            "hvc1" | "hev1" => Some("HEVC"),
            "av01" => Some("AV1"),
            "vp09" => Some("VP9"),
            _ => None,
        };
        if let Some(video) = video {
            conflicting_video |= hints
                .codec
                .as_deref()
                .is_some_and(|existing| existing != video);
            hints.codec = Some(video.into());
        }
        let audio = match codec {
            "mp4a.40.2" | "mp4a.40.5" | "mp4a.40.29" => Some("AAC"),
            "opus" => Some("Opus"),
            "ac-3" => Some("AC3"),
            "ec-3" => Some("EAC3"),
            _ => None,
        };
        if let Some(audio) = audio
            && !hints.audio.iter().any(|value| value == audio)
        {
            hints.audio.push(audio.into());
        }
    }
    if conflicting_video {
        hints.codec = None;
    }
    hints
}
