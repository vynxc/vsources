//! The handler pipeline: match, validate, transform, process, and remove.

// The pipeline ports `parse-torrent-title`'s index arithmetic verbatim:
// match offsets are `i64` with `-1` as the not-found sentinel, exactly
// like the TypeScript original. Every value is bounded by the input
// string's length, so wrap/truncation cannot occur.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;

use crate::handlers;
use crate::js_regex;
use crate::types::{Handler, MatchIndices, ParseMeta, ParsedTorrentTitle, VALUE_SET_FIELDS, Value};
use crate::utils;

/// Per-field parse metadata during a run.
pub(crate) type ParseResult = BTreeMap<&'static str, ParseMeta>;

/// Parse a release name with the default handler table.
pub fn parse_torrent_title(name: &str) -> ParsedTorrentTitle {
    parse(name, handlers::all())
}

/// Parse with the supplied handlers.
// The pipeline mirrors the upstream `parse` function statement for
// statement; splitting it would hide the port's structure.
#[allow(clippy::too_many_lines)]
pub(crate) fn parse(name: &str, handlers: &[Handler]) -> ParsedTorrentTitle {
    let mut title = name.to_string();
    if needs_whitespace_collapse(&title) {
        title = utils::collapse_whitespace(&title);
    }
    if title.contains('_') {
        title = title.replace('_', " ");
    }

    let mut end_of_title = title.len() as i64;
    // Byte index just past the episode marker in the mutating string; the
    // episode title, when present, starts here.
    let mut episode_title_start: i64 = -1;
    let mut episode_marker_seen = false;

    let mut result: ParseResult = BTreeMap::new();

    for handler in handlers {
        let field = handler.field;
        let mut skip_from_title = handler.skip_from_title;
        let m_found = result.contains_key(field);

        let mut meta = None;

        if let Some(pattern) = &handler.pattern {
            if m_found && !handler.keep_matching {
                continue;
            }
            let mut caps = pattern.captures(title.as_str()).ok().flatten();
            let Some(first) = caps
                .as_ref()
                .and_then(|c| c.get(0))
                .map(|m| (m.start(), m.end()))
            else {
                continue;
            };
            let raw_matched = title[first.0..first.1].to_string();

            // Take a later occurrence rather than dropping the handler.
            if let Some(retry) = &handler.retry_past_title
                && (first.0 as i64) < end_of_title
                && js_regex::is_match(retry, &raw_matched)
            {
                let mut next: Option<usize> = None;
                for m in retry.find_iter(&title).flatten() {
                    if (m.start() as i64) >= end_of_title {
                        next = Some(m.start());
                        break;
                    }
                }
                match next.and_then(|pos| {
                    pattern
                        .captures_from_pos(title.as_str(), pos)
                        .ok()
                        .flatten()
                        .filter(|c| c.get(0).is_some_and(|m| m.start() == pos))
                }) {
                    Some(later) => caps = Some(later),
                    None => continue,
                }
            }
            let Some(caps) = caps.as_ref() else { continue };
            let Some(m0) = caps.get(0) else { continue };
            let match_start = m0.start();
            let raw_matched = m0.as_str().to_string();

            // Validators (and matchGroup) read group positions.
            let idxs: Option<MatchIndices> = handler
                .validator
                .as_ref()
                .map(|_| utils::match_indices(caps));

            if let Some(validator) = &handler.validator {
                let idxs = idxs.clone().unwrap_or_else(|| utils::match_indices(caps));
                if !validator.check(&title, &idxs) {
                    continue;
                }
            }

            let mut should_skip = false;
            if handler.skip_if_first {
                // Skip when other fields matched earlier in the title and
                // nothing matched before this handler's match.
                let mut has_other = false;
                let mut has_before = false;
                for (f, fm) in &result {
                    if f != &field && !fm.m_value.is_empty() {
                        has_other = true;
                        if match_start >= fm.m_index {
                            has_before = true;
                            break;
                        }
                    }
                }
                should_skip = has_other && !has_before;
            }
            if should_skip {
                continue;
            }

            if !handler.skip_if_before.is_empty() {
                for skip_field in handler.skip_if_before {
                    if let Some(fm) = result.get(skip_field)
                        && match_start < fm.m_index
                    {
                        should_skip = true;
                        break;
                    }
                }
                if should_skip {
                    continue;
                }
            }

            // A disambiguator only reads as one when it is the last word of
            // the title; anything after it means the token belongs to the
            // name.
            if handler.must_end_title
                && ((match_start + raw_matched.len()) as i64) < end_of_title - 1
            {
                continue;
            }

            // Default to capture group 1 when value_group is unset.
            let matched_part = if caps.len() > 1 {
                match handler.value_group {
                    None | Some(0) => caps
                        .get(1)
                        .map(|g| g.as_str().to_string())
                        .unwrap_or_default(),
                    Some(vg) if caps.len() > vg => caps
                        .get(vg)
                        .map(|g| g.as_str().to_string())
                        .unwrap_or_default(),
                    _ => raw_matched.clone(),
                }
            } else {
                raw_matched.clone()
            };

            // A bracketed prologue belongs to the title, not the tag list.
            if title.starts_with('[')
                && let Some(before) = utils::before_title().captures(&title).ok().flatten()
                && before
                    .get(0)
                    .is_some_and(|g| g.as_str().contains(&raw_matched))
            {
                skip_from_title = true;
            }

            let mut work = if m_found {
                result.get(field).cloned().unwrap_or_else(|| {
                    ParseMeta::empty(if is_value_set_field(field) {
                        Some(Value::Set(Vec::new()))
                    } else {
                        None
                    })
                })
            } else {
                ParseMeta::empty(if is_value_set_field(field) {
                    Some(Value::Set(Vec::new()))
                } else {
                    None
                })
            };
            work.m_index = match_start;
            work.m_value.clone_from(&raw_matched);
            work.matched.push(raw_matched.clone());
            if !is_value_set_field(field) {
                work.value = Some(Value::Str(matched_part));
            }
            if let Some(mg) = handler.match_group {
                let idxs = idxs.clone().unwrap_or_else(|| utils::match_indices(caps));
                if let Some(Some((s, _))) = idxs.get(mg) {
                    work.m_index = *s;
                    work.m_value = caps
                        .get(mg)
                        .map(|g| g.as_str().to_string())
                        .unwrap_or_default();
                }
            }
            result.insert(field, work.clone());
            meta = Some(work);
        }

        if let Some(processor) = &handler.processor {
            let mut work = meta
                .clone()
                .or_else(|| result.get(field).cloned())
                .unwrap_or_else(|| ParseMeta::empty(None));
            processor.run(&title, &mut work, &result);
            if work.value.is_some() {
                result.insert(field, work.clone());
                meta = Some(work);
            } else {
                result.remove(field);
                meta = None;
            }
        }

        let Some(meta) = meta.as_mut() else { continue };

        if meta.value.is_some()
            && let Some(transform) = &handler.transform
        {
            transform.apply(&title, meta, &mut result);
            result.insert(field, meta.clone());
        }

        if meta.value.is_none() {
            result.remove(field);
            continue;
        }

        if !result.contains_key(field)
            || (meta.processed && !handler.keep_matching && !is_value_set_field(field))
        {
            continue;
        }

        // Only the first episode marker can anchor an episode title, and
        // only if it stands on its own.
        let is_first_episode_marker =
            field == "episodes" && !episode_marker_seen && !meta.m_value.is_empty();
        let mut marker_anchors_title = false;
        if is_first_episode_marker {
            let leading_bracket = if title.starts_with('[') {
                utils::before_title().captures(&title).ok().flatten()
            } else {
                None
            };
            // A marker whose text begins inside a word is a mis-parse; a
            // digit before it is normal.
            let starts_mid_word = meta.m_index > 0
                && title[..meta.m_index]
                    .chars()
                    .last()
                    .is_some_and(char::is_alphabetic)
                && meta
                    .m_value
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            let inside_leading_bracket = leading_bracket
                .as_ref()
                .and_then(|b| b.get(0))
                .is_some_and(|g| meta.m_index < g.as_str().len());
            marker_anchors_title = !starts_mid_word && !inside_leading_bracket;
        }

        let removed = handler.remove || meta.remove;
        if removed {
            meta.remove = true;
            // Keep the episode-title anchor aligned with the mutating
            // string.
            if episode_title_start >= 0 && (meta.m_index as i64) < episode_title_start {
                episode_title_start =
                    if (meta.m_index + meta.m_value.len()) as i64 <= episode_title_start {
                        episode_title_start - meta.m_value.len() as i64
                    } else {
                        meta.m_index as i64
                    };
            }
            let remove_end = (meta.m_index + meta.m_value.len()).min(title.len());
            if meta.m_index <= remove_end && meta.m_index <= title.len() {
                title.replace_range(meta.m_index..remove_end, "");
                // Match metadata belongs to the mutable title, not its old
                // spelling. Keeping stale byte offsets can land a later
                // processor inside an emoji after earlier tags are removed.
                for prior in result.values_mut() {
                    if prior.m_index >= remove_end {
                        prior.m_index -= remove_end - meta.m_index;
                    } else if prior.m_index > meta.m_index {
                        prior.m_index = meta.m_index;
                    }
                }
            }
        }

        if is_first_episode_marker {
            episode_marker_seen = true;
            if marker_anchors_title {
                episode_title_start = if removed {
                    meta.m_index as i64
                } else {
                    (meta.m_index + meta.m_value.len()) as i64
                };
            }
        }

        if !skip_from_title && meta.m_index != 0 && (meta.m_index as i64) < end_of_title {
            end_of_title = meta.m_index as i64;
        }

        if meta.remove
            && (skip_from_title || meta.m_index == 0)
            && (meta.m_index as i64) < end_of_title
        {
            end_of_title -= meta.m_value.len() as i64;
        }

        meta.remove = false;
        meta.processed = true;
        result.insert(field, meta.clone());
    }

    assemble(&result, &title, end_of_title, episode_title_start)
}

/// Build the final [`ParsedTorrentTitle`] from the per-field metadata.
#[allow(clippy::too_many_lines)]
fn assemble(
    result: &ParseResult,
    title: &str,
    end_of_title: i64,
    episode_title_start: i64,
) -> ParsedTorrentTitle {
    let mut out = ParsedTorrentTitle::default();
    let mut languages: Option<Vec<String>> = None;

    for (field, meta) in result {
        let Some(value) = meta.value.clone() else {
            continue;
        };
        if let Value::Set(set) = &value {
            let mut values = set.clone();
            if *field == "languages" && values.iter().any(|l| l == "es-419") {
                // Latin American Spanish supersedes the generic tag.
                values.retain(|l| l != "es");
            }
            match *field {
                "audio" => out.audio = Some(values),
                "channels" => out.channels = Some(values),
                "editions" => out.editions = Some(values),
                "hdr" => out.hdr = Some(values),
                "languages" => languages = Some(values),
                "releaseTypes" => out.release_types = Some(values),
                _ => {
                    out.extra
                        .insert(field.to_string(), crate::types::ExtraValue::Strs(values));
                }
            }
            continue;
        }
        match value {
            Value::Str(v) => match *field {
                "title" => out.title = Some(v),
                "year" => out.year = Some(v),
                "date" => out.date = Some(v),
                "country" => out.country = Some(v),
                "resolution" => out.resolution = Some(v),
                "quality" => out.quality = Some(v),
                "codec" => out.codec = Some(v),
                "bitDepth" => out.bit_depth = Some(v),
                "threeD" => out.three_d = Some(v),
                "episodeCode" => out.episode_code = Some(v),
                "episodeTitle" => out.episode_title = Some(v),
                "group" => out.group = Some(v),
                "site" => out.site = Some(v),
                "network" => out.network = Some(v),
                "container" => out.container = Some(v),
                "extension" => out.extension = Some(v),
                "region" => out.region = Some(v),
                "size" => out.size = Some(v),
                _ => {
                    out.extra
                        .insert(field.to_string(), crate::types::ExtraValue::Str(v));
                }
            },
            Value::Bool(b) => match *field {
                "complete" => out.complete = Some(b),
                "dubbed" => out.dubbed = Some(b),
                "subbed" => out.subbed = Some(b),
                "hardcoded" => out.hardcoded = Some(b),
                "repack" => out.repack = Some(b),
                "proper" => out.proper = Some(b),
                "retail" => out.retail = Some(b),
                "regraded" => out.regraded = Some(b),
                "unrated" => out.unrated = Some(b),
                "uncensored" => out.uncensored = Some(b),
                "extended" => out.extended = Some(b),
                "convert" => out.convert = Some(b),
                "documentary" => out.documentary = Some(b),
                "commentary" => out.commentary = Some(b),
                "upscaled" => out.upscaled = Some(b),
                "ppv" => out.ppv = Some(b),
                _ => {
                    out.extra
                        .insert(field.to_string(), crate::types::ExtraValue::Bool(b));
                }
            },
            Value::Ints(v) => match *field {
                "seasons" => out.seasons = Some(v),
                "episodes" => out.episodes = Some(v),
                "volumes" => out.volumes = Some(v),
                _ => {
                    out.extra
                        .insert(field.to_string(), crate::types::ExtraValue::Ints(v));
                }
            },
            // Value sets were consumed above; kept for exhaustiveness.
            Value::Set(_) => {}
        }
    }
    out.languages = languages;

    let title_end = end_of_title.clamp(0, title.len() as i64) as usize;
    let mut raw_title = title[..title_end].to_string();
    if out.episodes.as_ref().is_some_and(|e| !e.is_empty()) {
        raw_title = js_regex::replace_all_str(utils::trailing_episode(), &raw_title, "");
    }
    out.title = Some(utils::clean_title(&raw_title));

    // A bare-number episode marker gives no confidence that adjacent words
    // are an episode title, so require a marker with structure: any
    // non-digit char. A marker ending in a letter means its pattern bit
    // into the following word, so what follows is mangled and unusable.
    let episodes_meta = result.get("episodes");
    if out.episodes.as_ref().is_some_and(|e| !e.is_empty())
        && episodes_meta.is_some_and(|em| {
            em.m_value.chars().any(|c| !c.is_ascii_digit())
                && !em.m_value.chars().last().is_some_and(char::is_alphabetic)
        })
        && episode_title_start > 0
        && (episode_title_start as usize) < title.len()
        && let Some(episode_title) =
            utils::extract_episode_title(title, episode_title_start as usize, out.group.as_deref())
    {
        out.episode_title = Some(episode_title);
    }
    out
}

/// Whether the field accumulates values in an ordered set.
fn is_value_set_field(field: &str) -> bool {
    VALUE_SET_FIELDS.contains(&field)
}

/// Whitespace that `\s+` to `" "` collapsing would actually change.
fn needs_whitespace_collapse(s: &str) -> bool {
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if prev_ws || c != ' ' {
                return true;
            }
            prev_ws = true;
        } else {
            prev_ws = false;
        }
    }
    false
}

#[cfg(test)]
mod unicode_tests {
    use super::*;
    #[test]
    fn unicode_provider_labels_keep_offsets_aligned_after_removing_tags() {
        let label = "Inception (2010) — 🎬 Inception - (2010)\n🔥 1080p | 🌍 Original Audio | 🎧 AAC\n🎞️ M3U8 | ⏱️ 90 min\n💧 Hydrogen | 🔗 Provider: quietridge.top";
        for prefix in ["", "日本語 · ", "é🎞️ "] {
            let parsed = parse_torrent_title(&format!("{prefix}{label}"));
            assert_eq!(parsed.year.as_deref(), Some("2010"));
            assert_eq!(parsed.resolution.as_deref(), Some("1080p"));
            assert!(
                parsed
                    .audio
                    .as_ref()
                    .is_some_and(|a| a.iter().any(|v| v.contains("AAC")))
            );
        }
    }
}
