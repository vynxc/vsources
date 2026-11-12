use std::collections::HashSet;
use url::Url;
use vsources_core::types::{Stream, StreamMeta};

use super::{FLIXCLOUD, ID, LABEL, ReanimeStream, TTL};

pub(super) fn stream_name(server: &str, quality: &str, category: &str) -> String {
    let mut parts = vec![format!("ReAnime [{server}]")];
    if !quality.is_empty() {
        parts.push(quality.to_owned());
    }
    if !category.is_empty() {
        parts.push(category.to_owned());
    }
    parts.join(" | ")
}

pub(super) fn build_cards(streams: &[ReanimeStream], display_title: &str) -> Vec<Stream> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for stream in streams {
        if !stream.url.starts_with("http") || !seen.insert(stream.url.clone()) {
            continue;
        }
        let mut meta = StreamMeta {
            resolution: explicit_height(&stream.quality),
            source_id: Some(ID.to_string()),
            source_label: Some(LABEL.to_string()),
            ..StreamMeta::default()
        };
        meta.request_headers
            .insert("Referer".to_string(), format!("{FLIXCLOUD}/"));
        meta.request_headers
            .insert("Origin".to_string(), FLIXCLOUD.to_string());
        let mut card = Stream {
            url: Url::parse(&stream.url).unwrap_or_else(|error| panic!("valid media URL: {error}")),
            format: stream.format,
            label: Some(format!("{display_title} — [ReAnime {}]", stream.name)),
            meta,
            ttl: TTL,
            is_external: false,
            behavior_hints: std::collections::BTreeMap::new(),
        };
        if !stream.xor_key_b64.is_empty() {
            card.behavior_hints
                .insert("reanimeXorKey".to_string(), stream.xor_key_b64.clone());
        }
        out.push(card);
    }
    out
}

fn explicit_height(value: &str) -> Option<u16> {
    let value = value.split('|').next()?.trim();
    if value.eq_ignore_ascii_case("4k") {
        return Some(2160);
    }
    if let Some((width, height)) = value.split_once(['x', 'X']) {
        if width.trim().parse::<u16>().ok()? == 0 {
            return None;
        }
        return height.trim().parse().ok().filter(|height| *height > 0);
    }
    value
        .strip_suffix('p')
        .or_else(|| value.strip_suffix('P'))
        .unwrap_or(value)
        .parse()
        .ok()
        .filter(|height| *height > 0)
}
