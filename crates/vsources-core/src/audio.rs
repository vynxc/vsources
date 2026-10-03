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
/// Read ISO-639 audio tags from a complete MPEG-TS program-map section.
///
/// The input is a bounded segment prefix. Incomplete sections, unsupported
/// private audio types and non-TS data are inconclusive. No media is decoded.
/// Entries follow the program-map audio order, matching demuxed audio indices;
/// an empty string means that audio stream has no language descriptor.
#[must_use]
pub fn mpegts_audio_languages(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.first() != Some(&0x47) {
        return None;
    }
    for packet in bytes.as_chunks::<188>().0 {
        if packet[0] != 0x47 {
            return None;
        }
        if packet[1] & 0x40 == 0 || packet[3] & 0x10 == 0 {
            continue;
        }
        let mut position = 4;
        if packet[3] & 0x20 != 0 {
            position += 1 + usize::from(*packet.get(position)?);
        }
        position += 1 + usize::from(*packet.get(position)?);
        let Some(section) = packet.get(position..) else {
            continue;
        };
        if section.first() != Some(&2) || section.len() < 12 {
            continue;
        }
        let length = (usize::from(section[1] & 15) << 8) | usize::from(section[2]);
        let end = 3_usize.checked_add(length)?.checked_sub(4)?;
        if end > section.len() || section[5] & 1 == 0 {
            continue;
        }
        let info_length = (usize::from(section[10] & 15) << 8) | usize::from(section[11]);
        let mut cursor = 12_usize.checked_add(info_length)?;
        let mut audio = Vec::new();
        while cursor < end {
            let header = section.get(cursor..cursor.checked_add(5)?)?;
            let descriptor_length = (usize::from(header[3] & 15) << 8) | usize::from(header[4]);
            let next = cursor.checked_add(5)?.checked_add(descriptor_length)?;
            if next > end {
                return None;
            }
            let descriptors = section.get(cursor + 5..next)?;
            let mut language = String::new();
            let mut private_audio = false;
            let mut offset = 0;
            while offset < descriptors.len() {
                let tag = *descriptors.get(offset)?;
                let size = usize::from(*descriptors.get(offset + 1)?);
                let body = descriptors.get(offset + 2..offset.checked_add(2 + size)?)?;
                if tag == 10 && body.len() >= 4 && body[..3].iter().all(u8::is_ascii_alphabetic) {
                    language = std::str::from_utf8(&body[..3]).ok()?.to_ascii_lowercase();
                }
                if matches!(tag, 0x6a | 0x7a | 0x7b | 0x7c)
                    || (tag == 5
                        && [
                            b"AC-3".as_slice(),
                            b"EAC3",
                            b"DTS1",
                            b"DTS2",
                            b"DTS3",
                            b"Opus",
                        ]
                        .contains(&body))
                {
                    private_audio = true;
                }
                offset += 2 + size;
            }
            match header[0] {
                3 | 4 | 15 | 17 | 0x81 | 0x87 => audio.push(language),
                6 if private_audio => audio.push(language),
                6 if !language.is_empty() => return None,
                _ => {}
            }
            cursor = next;
        }
        if !audio.is_empty() {
            return Some(audio);
        }
    }
    None
}

#[cfg(test)]
mod ts_tests {
    use super::mpegts_audio_languages;

    fn packet() -> Vec<u8> {
        let mut section = vec![2, 0xb0, 0, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0];
        // Video precedes Hindi and English audio; video must not shift indices.
        section.extend([27, 0xe1, 0, 0xf0, 0]);
        for (pid, language) in [(1, b"hin"), (2, b"eng")] {
            section.extend([15, 0xe1, pid, 0xf0, 6, 10, 4]);
            section.extend(language);
            section.push(0);
        }
        section.extend([0; 4]);
        section[2] = u8::try_from(section.len() - 3).unwrap_or(0);
        let mut data = vec![0x47, 0x41, 0, 0x10, 0];
        data.extend(section);
        data.resize(188, 0xff);
        data
    }

    #[test]
    fn reads_real_descriptors_in_audio_order_and_rejects_incomplete_data() {
        let data = packet();
        assert_eq!(
            mpegts_audio_languages(&data),
            Some(vec!["hin".into(), "eng".into()])
        );
        assert_eq!(mpegts_audio_languages(&data[..100]), None);
        assert_eq!(mpegts_audio_languages(b"not a TS packet eng"), None);
        let mut corrupt = data.clone();
        corrupt[3] = 0x30;
        corrupt[4] = 255;
        assert_eq!(mpegts_audio_languages(&corrupt), None);
        let mut incomplete = data;
        incomplete[7] = 255;
        assert_eq!(mpegts_audio_languages(&incomplete), None);
    }
}
