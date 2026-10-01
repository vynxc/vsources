//! Bounded audio-track inspection without downloading or decoding a movie.

/// Find an explicitly tagged English audio stream in a Matroska/WebM prefix.
///
/// Reads EBML structure, not arbitrary `eng` strings in file data. Incomplete
/// track metadata, unsupported containers and untagged audio are inconclusive.
/// The result is a zero-based audio-stream index, independent of video/subtitles.
#[must_use]
pub fn matroska_english_audio_index(bytes: &[u8]) -> Option<u32> {
    let header = element(bytes, 0)?;
    if header.id != 0x1a45_dfa3 {
        return None;
    }
    let segment = element(bytes, header.end)?;
    if segment.id != 0x1853_8067 {
        return None;
    }
    let mut position = segment.start;
    while position < bytes.len().min(segment.end) {
        let entry = element(bytes, position)?;
        if entry.id == 0x1654_ae6b {
            return english_track(bytes.get(entry.start..entry.end)?);
        }
        if entry.id == 0x1f43_b675 || entry.end <= position {
            return None;
        }
        position = entry.end;
    }
    None
}

struct Element {
    id: u64,
    start: usize,
    end: usize,
}

fn element(bytes: &[u8], position: usize) -> Option<Element> {
    let (id, id_len) = vint(bytes.get(position..)?, false, 4)?;
    let (size, size_len) = vint(bytes.get(position.checked_add(id_len)?..)?, true, 8)?;
    let start = position.checked_add(id_len)?.checked_add(size_len)?;
    let unknown = size == (1_u64 << (size_len * 7)) - 1;
    // Unknown sizes are legal on the enclosing Segment, not our Tracks tree.
    let end = if unknown && id == 0x1853_8067 {
        bytes.len()
    } else if unknown {
        return None;
    } else {
        start.checked_add(usize::try_from(size).ok()?)?
    };
    Some(Element { id, start, end })
}

fn vint(bytes: &[u8], strip_marker: bool, maximum: usize) -> Option<(u64, usize)> {
    let first = *bytes.first()?;
    let length = usize::try_from(first.leading_zeros())
        .ok()?
        .checked_add(1)?;
    if length > maximum {
        return None;
    }
    let raw = bytes.get(..length)?;
    let mut value = u64::from(if strip_marker {
        first & u8::try_from(0xff_u16 >> length).ok()?
    } else {
        first
    });
    for byte in &raw[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, length))
}

fn english_track(tracks: &[u8]) -> Option<u32> {
    let mut position = 0;
    let mut audio_index = 0_u32;
    let mut selected = None;
    while position < tracks.len() {
        let entry = element(tracks, position)?;
        let body = tracks.get(entry.start..entry.end)?;
        if entry.id == 0xae {
            let mut field_position = 0;
            let mut track_type = None;
            let mut language = None;
            let mut ietf = None;
            let mut enabled = true;
            while field_position < body.len() {
                let field = element(body, field_position)?;
                let raw = body.get(field.start..field.end)?;
                match field.id {
                    0x83 if raw.len() == 1 => track_type = raw.first().copied(),
                    0xb9 if raw.len() == 1 => enabled = raw[0] != 0,
                    0x0022_b59c => language = std::str::from_utf8(raw).ok(),
                    0x0022_b59d => ietf = std::str::from_utf8(raw).ok(),
                    _ => {}
                }
                field_position = field.end;
            }
            if track_type == Some(2) {
                if enabled
                    && ietf.or(language).is_some_and(|language| {
                        let language = language.trim_matches('\0');
                        language.eq_ignore_ascii_case("eng")
                            || language.eq_ignore_ascii_case("en")
                            || language
                                .get(..3)
                                .is_some_and(|s| s.eq_ignore_ascii_case("en-"))
                    })
                    && selected.is_none()
                {
                    selected = Some(audio_index);
                }
                audio_index = audio_index.checked_add(1)?;
            }
        }
        position = entry.end;
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &[u8], body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 127);
        [
            id,
            &[0x80 | u8::try_from(body.len()).unwrap_or_default()],
            body,
        ]
        .concat()
    }

    fn track(kind: u8, language: Option<&str>, ietf: Option<&str>) -> Vec<u8> {
        let mut fields = node(&[0x83], &[kind]);
        if let Some(language) = language {
            fields.extend(node(&[0x22, 0xb5, 0x9c], language.as_bytes()));
        }
        if let Some(language) = ietf {
            fields.extend(node(&[0x22, 0xb5, 0x9d], language.as_bytes()));
        }
        node(&[0xae], &fields)
    }

    fn file(tracks: &[u8]) -> Vec<u8> {
        let mut bytes = node(&[0x1a, 0x45, 0xdf, 0xa3], &[]);
        bytes.extend([0x18, 0x53, 0x80, 0x67, 0xff]);
        bytes.extend(node(&[0x16, 0x54, 0xae, 0x6b], tracks));
        bytes
    }

    #[test]
    fn indexes_audio_only_and_honors_ietf_language() {
        let tracks = [
            track(1, None, None),
            track(2, Some("jpn"), None),
            track(17, Some("eng"), None),
            track(2, Some("jpn"), Some("en-US")),
        ]
        .concat();
        assert_eq!(matroska_english_audio_index(&file(&tracks)), Some(1));
    }

    #[test]
    fn rejects_unmarked_audio_false_strings_and_incomplete_tracks() {
        let bytes = file(&[track(2, None, None), track(17, Some("eng"), None)].concat());
        assert_eq!(matroska_english_audio_index(&bytes), None);
        let bytes = file(&track(2, Some("eng"), None));
        for end in 0..bytes.len() {
            assert_eq!(matroska_english_audio_index(&bytes[..end]), None);
        }
        assert_eq!(matroska_english_audio_index(&bytes), Some(0));
        assert_eq!(matroska_english_audio_index(b"not a container eng"), None);
    }
}
