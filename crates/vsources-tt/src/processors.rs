//! Post-match processors for the handler pipeline.

use crate::js_regex;
use crate::types::{ParseMeta, ParseResult, Value};
use crate::utils;

/// Processors mirroring the TypeScript handler `process` closures.
#[derive(Debug, Clone)]
pub enum Processor {
    /// Remove a pattern from the current string value.
    RemoveFromValue(String),
    /// Detect `vol(ume) N` markers for the volumes field.
    Volumes,
    /// Fallback episode detection from dash/bracket markers.
    EpisodesFallback,
    /// Portuguese/Spanish episode language fallback.
    PortugueseLanguages,
    /// Set `subbed` when the languages set contains "multi subs".
    SubbedFromLanguages,
    /// Set `dubbed` when the languages set contains multi/dual audio.
    DubbedFromLanguages,
    /// Extract the release group from the working title.
    Group,
}

impl Processor {
    /// Run this processor over the meta for its field.
    pub(crate) fn run(&self, title: &str, meta: &mut ParseMeta, result: &ParseResult) {
        match self {
            Self::RemoveFromValue(pattern) => {
                if let Some(Value::Str(v)) = &meta.value
                    && !v.is_empty()
                    && let Some(re) = js_regex::compile_ci(pattern)
                {
                    meta.value = Some(Value::Str(js_regex::replace_all_str(&re, v, "")));
                }
            }
            Self::Volumes => run_volumes(title, meta, result),
            Self::EpisodesFallback => run_episodes_fallback(title, meta, result),
            Self::PortugueseLanguages => run_portuguese_languages(title, meta, result),
            Self::SubbedFromLanguages => {
                if let Some(lm) = result.get("languages")
                    && utils::value_set_contains(lm, "multi subs")
                {
                    meta.value = Some(Value::Bool(true));
                }
            }
            Self::DubbedFromLanguages => {
                if let Some(lm) = result.get("languages")
                    && (utils::value_set_contains(lm, "multi audio")
                        || utils::value_set_contains(lm, "dual audio"))
                {
                    meta.value = Some(Value::Bool(true));
                }
            }
            Self::Group => {
                if meta.value.is_some() {
                    return;
                }
                match utils::extract_group(title, result) {
                    Some((value, index)) => {
                        meta.value = Some(Value::Str(value.clone()));
                        meta.m_value = value;
                        meta.m_index = index;
                    }
                    None => {
                        meta.value = None;
                    }
                }
            }
        }
    }
}

/// `vol(ume) N` detection after the year marker.
fn run_volumes(title: &str, meta: &mut ParseMeta, result: &ParseResult) {
    let Some(re) = js_regex::compile_ci(r"\bvol(?:ume)?[. -]*(\d{1,3})") else {
        return;
    };
    let start_index = result
        .get("year")
        .map_or(0, |yr| yr.m_index.min(title.len()));
    let substring = &title[start_index..];
    let Some(caps) = re.captures(substring).ok().flatten() else {
        return;
    };
    let Some(num_text) = caps.get(1).map(|m| m.as_str()) else {
        return;
    };
    let Some(num) = num_text.parse::<i64>().ok() else {
        return;
    };
    meta.m_index = start_index + caps.get(0).map_or(0, |m| m.start());
    meta.m_value = caps
        .get(0)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    meta.value = Some(Value::Ints(vec![num]));
    meta.remove = true;
}

/// Fallback episode detection: dash/bracket markers, trailing numbers, and
/// bare leading numbers before the technical fields.
// Mirrors the upstream processor statement for statement.
#[allow(clippy::too_many_lines)]
fn run_episodes_fallback(title: &str, meta: &mut ParseMeta, result: &ParseResult) {
    if meta.value.is_some() {
        return;
    }
    let bt_re = js_regex::compile_ci(
        r"(?:movie\W*|film\W*|^)?(?:[ .]+-[ .]+|[(\[][ .]*)(\d{1,4})(?:a|b|v\d|\.\d)?(?:\W|$)(?:movie|film|\d+)?",
    );
    let bt_re_neg_before =
        js_regex::compile_ci(r"(?:movie\W*|film\W*)(?:[ .]+-[ .]+|[(\[][ .]*)(\d{1,4})");
    let bt_re_neg_after =
        js_regex::compile_ci(r"(?:movie|film)|(\d{1,4})(?:a|b|v\d|\.\d)(?:\W)(?:\d+)");
    let mt_re = js_regex::compile_ci(
        r"^(?:[(\[-][ .]?)?(\d{1,4})(?:a|b|v\d)?(?:\Wmovie|\Wfilm|-\d)?(?:\W|$)",
    );
    let mt_re_neg_after = js_regex::compile_ci(r"(\d{1,4})(?:a|b|v\d)?(?:\Wmovie|\Wfilm|-\d)");
    let common_resolution_neg = js_regex::compile_ci(r"\[(?:480|720|1080)\]");
    let common_fps_neg = js_regex::compile_ci(r"\d+(?:fps|帧率?)");

    let mut start_index = 0;
    for component in ["year", "seasons"] {
        if let Some(cm) = result.get(component)
            && cm.m_index > 0
            && (start_index == 0 || cm.m_index < start_index)
        {
            start_index = cm.m_index;
        }
    }
    let mut end_index = title.len();
    for component in ["resolution", "quality", "codec", "audio"] {
        if let Some(cm) = result.get(component)
            && cm.m_index > 0
            && cm.m_index < end_index
        {
            end_index = cm.m_index;
        }
    }

    let beginning_title = &title[..end_index];
    start_index = start_index.min(title.len());
    let middle_title = &title[start_index..end_index.max(start_index)];

    let mut m_str = String::new();
    let bt_caps = bt_re
        .as_ref()
        .and_then(|re| re.captures(beginning_title).ok().flatten());
    if let (Some(_bt_re), Some(caps)) = (bt_re.as_ref(), bt_caps) {
        let m0 = caps.get(0);
        let match_index = m0.map_or(0, |m| m.start());
        let match_end = m0.map_or(0, |m| m.end());
        let mut full = m0.map(|m| m.as_str().to_string()).unwrap_or_default();
        let next_char_is_word_char = match_end >= beginning_title.len()
            && end_index < title.len()
            && title[end_index..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let neg = bt_re_neg_before
            .as_ref()
            .is_some_and(|re| js_regex::is_match(re, &full))
            || bt_re_neg_after
                .as_ref()
                .is_some_and(|re| js_regex::is_match(re, &full))
            || common_resolution_neg
                .as_ref()
                .is_some_and(|re| js_regex::is_match(re, &full))
            || common_fps_neg
                .as_ref()
                .is_some_and(|re| js_regex::is_match(re, &full));
        if match_index == 0 || neg || next_char_is_word_char {
            // rejected
        } else if let Some(g1) = caps.get(1) {
            full = g1.as_str().to_string();
            m_str = full;
        }
    }

    // Check for 2-3 digit episode at the end of the title (right before the
    // resolution/quality/codec fields).
    if m_str.is_empty() && end_index > 0 && end_index < title.len() {
        let Some(dot_re) = js_regex::compile_ci(r"[ ._](\d{2,3})(?:[ ._]?v\d)?[ ._(\[]*$") else {
            return;
        };
        let end_section = &title[..end_index];
        if let Some(caps) = dot_re.captures(end_section).ok().flatten() {
            let cap_start = caps.get(1).map_or(0, |m| m.start());
            let cap_text = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            // Skip digits that belong to the season match.
            let overlaps_seasons = result.get("seasons").is_some_and(|sm| {
                !sm.m_value.is_empty()
                    && cap_start >= sm.m_index
                    && cap_start < sm.m_index + sm.m_value.len()
            });
            // Skip when the year sat between this number and the technical
            // fields.
            let year_in_between = result
                .get("year")
                .is_some_and(|ym| ym.m_index >= cap_start && ym.m_index <= end_index);
            if !overlaps_seasons && !year_in_between {
                m_str = cap_text.to_string();
            }
        }
    }

    if m_str.is_empty()
        && let Some(mt_re) = mt_re.as_ref()
        && let Some(caps) = mt_re.captures(middle_title).ok().flatten()
        && let Some(g1) = caps.get(1)
    {
        let from_capture = &middle_title[g1.start()..];
        let neg_after = mt_re_neg_after
            .as_ref()
            .is_some_and(|re| js_regex::is_match(re, from_capture));
        if !neg_after {
            m_str = g1.as_str().to_string();
        }
    }

    if !m_str.is_empty() {
        m_str.retain(|c| c.is_ascii_digit());
        let year_duplicate = result.get("year").is_some_and(|ym| {
            let value = match &ym.value {
                Some(Value::Str(s)) => s.clone(),
                Some(Value::Bool(b)) => b.to_string(),
                Some(Value::Ints(v)) => v
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                Some(Value::Set(v)) => v.join(","),
                None => String::new(),
            };
            value == m_str
        });
        if let Ok(ep) = m_str.parse::<i64>()
            && !year_duplicate
        {
            meta.m_index = title.find(&m_str).unwrap_or(0);
            meta.m_value.clone_from(&m_str);
            meta.value = Some(Value::Ints(vec![ep]));
        }
    }
}

/// Portuguese/Spanish episode markers imply a `pt` language tag.
fn run_portuguese_languages(title: &str, meta: &mut ParseMeta, result: &ParseResult) {
    let ere = js_regex::compile_ci(r"capitulo|ao");
    let tre = js_regex::compile_ci(r"dublado");

    meta.m_index = 0;
    meta.m_value = String::new();

    if let Some(Value::Set(set)) = &meta.value
        && set.iter().any(|v| v == "pt" || v == "es")
    {
        return;
    }

    let episodes_marker = result
        .get("episodes")
        .filter(|em| !em.m_value.is_empty())
        .and_then(|em| ere.as_ref().map(|re| (re, em.m_value.clone())))
        .is_some_and(|(re, mv)| js_regex::is_match(re, &mv));
    let dubbed = tre.as_ref().is_some_and(|re| js_regex::is_match(re, title));
    if episodes_marker || dubbed {
        match &mut meta.value {
            Some(Value::Set(set)) => {
                if !set.iter().any(|v| v == "pt") {
                    set.push("pt".to_string());
                }
            }
            _ => {
                meta.value = Some(Value::Set(vec!["pt".to_string()]));
            }
        }
    }
}
