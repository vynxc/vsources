//! Title cleanup, episode-title extraction, and release-group extraction.
//!
//! Ports the TypeScript `utils.ts` helpers that run after the handler
//! pipeline. All indices are byte offsets into UTF-8 strings; the original
//! used UTF-16 offsets, which is equally self-consistent for every splice
//! and comparison performed here.

use std::sync::OnceLock;

use fancy_regex::Captures;

use crate::js_regex::{self, Regex};
use crate::types::{MatchIndices, ParseMeta, ParseResult, Value};

/// File extensions recognized at the end of release names.
const EXTENSIONS: &str = "3g2|3gp|avi|flv|mkv|mk3d|mov|mp2|mp4|m4v|mpe|mpeg|mpg|mpv|webm|wmv|ogm|divx|ts|m2ts|iso|vob|sub|idx|ttxt|txt|smi|srt|ssa|ass|vtt|nfo|html";

/// Non-Latin character ranges (Hiragana, Katakana, CJK, Cyrillic).
const NEC: &str = r"\u{3040}-\u{309F}\u{30A0}-\u{30FF}\u{4E00}-\u{9FFF}\u{0400}-\u{04FF}";

/// Token pattern used for release group extraction.
const GROUP_TOKEN: &str = r"[^\s.\-\[\]()/\\]+";

macro_rules! cached_regex {
    ($name:ident, $pattern:expr) => {
        cached_regex!($name, $pattern, false);
    };
    ($name:ident, $pattern:expr, $ci:expr) => {
        pub(crate) fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| {
                js_regex::compile($pattern, $ci).unwrap_or_else(js_regex::never_match)
            })
        }
    };
}

cached_regex!(movie_indicator, r"[[(]movie[)\]]", true);
cached_regex!(release_group_marking_start, r"^[[【★].*[\]】★][ .]?(.+)");
cached_regex!(release_group_marking_end, r"(.+)[ .]?[[【★].*[\]】★]$");
cached_regex!(before_title, r"^\[([^[\]]+)\]");
cached_regex!(
    russian_cast,
    &format!(r"(\([^)]*[{NEC}][^)]*\))$|(?:/.*?)(\(.*\))$")
);
cached_regex!(
    alt_titles,
    &format!(r"[^/|(]*[{NEC}][^/|]*[/|]|[/|][^/|(]*[{NEC}][^/|]*")
);
cached_regex!(
    not_only_non_english,
    &format!(r"(?:[a-zA-Z][^{NEC}]+)([{NEC}].*[{NEC}])|([{NEC}].*[{NEC}])(?:[^{NEC}]+[a-zA-Z])")
);
cached_regex!(
    not_allowed_symbols,
    &format!(r"^[^\w{NEC}#[【★]+|[ \-:/\\[|{{(#$&^]+$")
);
cached_regex!(
    remaining_not_allowed,
    &format!(r"^[^\w{NEC}#]+|[\[\]({{}} ]+$")
);
cached_regex!(redundant_symbols_at_end, r"[ \-:./\\]+$");
cached_regex!(trailing_episode, r"[ .]+-[ .]*\d{1,4}[ .]*$");
cached_regex!(dots_to_spaces, r"(?<!\d)\.|\.(?!\d)");
cached_regex!(underscores, r"_+");
cached_regex!(surrounding_punctuation, r"^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$");
cached_regex!(episode_title_token, r"[^ .[\]() {}/\\|]+");
cached_regex!(extension_token, &format!("^({EXTENSIONS})$"), true);
cached_regex!(
    episode_hard_stop,
    r"^(?:complete[sd]?|completas?|incomplete|final|multi|dual|integrale?|nordic|true?french|subs?|subbed|subtitles?|legendas?|legendad[oa]|subtitulad[oa]|vostfr|vosta|dubbed|dublado|doblado)$",
    true
);
cached_regex!(
    episode_language_word,
    r"^(?:french|fran(?:c|ç)ais|german|deutsch|spanish|espa(?:n|ñ)ol|castellano|latino|italian|italiano|portuguese|portugu(?:e|ê)s|brazilian|dutch|nederlands|english|ingles|swedish|svenska?|norwegian|norska?|danish|danska?|finnish|finska?|suomi|greek|polish|polski|czech|russian|japanese|korean|chinese|hindi|tamil|telugu|arabic|hebrew|turkish|thai)$",
    true
);
cached_regex!(
    episode_trailing_tag,
    r"^(?:\d{3,4}[pi]|[48]k|u?hd|dl|synced)$",
    true
);
cached_regex!(episode_letter, &format!(r"[a-zA-Z{NEC}]"));
cached_regex!(version_token, r"^(?:v\d{1,2}|rs\d{1,2})$", true);
cached_regex!(resolution_ish, r"^(?:\d{3,4}[pi]|[48]k)$", true);
cached_regex!(of_remainder, r"^(?:of|из|iz) \d+$", true);
cached_regex!(ep_remainder, r"^ep(?:isode)?s?[ .]*\d{1,4}$", true);
cached_regex!(
    extension_with_lang,
    &format!(r"(?:[. ][a-z]{{2,3}}(?:-[a-z]{{2}})?)?[. ](?:{EXTENSIONS})$"),
    true
);
cached_regex!(sep_end, r"[\s.]$");
cached_regex!(leading_group_messy, r"^[\s.\-_]|[\s.\-_]$|[\s.\-_]{2}");
cached_regex!(trailing_bracket, r"\s?[\[(]([^\[\]()]*)[\])]$");
cached_regex!(bracket_group, &format!(r"-[\s.]?({GROUP_TOKEN})[\])]$"));
cached_regex!(last_token, &format!(r"({GROUP_TOKEN})$"));
cached_regex!(
    trailing_group,
    &format!(r"(?:([\s.])-[\s.]|-)({GROUP_TOKEN})$")
);
cached_regex!(
    episode_like,
    r"^(?:\d+|\d+[a-z]|\d+v\d+|\d+x\d+|s\d+\w*|ep?\d+\w*)$",
    true
);

/// Three-letter language codes as releases spell them (case-sensitive).
const LANGUAGE_CODES: &[&str] = &[
    "ENG", "ITA", "GER", "DEU", "FRE", "FRA", "SPA", "ESP", "POR", "RUS", "JPN", "JAP", "KOR",
    "CHI", "DUT", "NLD", "SWE", "NOR", "DAN", "FIN", "POL", "CZE", "GRE", "TUR", "ARA", "HEB",
    "HIN", "TAM", "TEL", "THA", "VIE", "HUN", "UKR", "BUL", "HRV", "SRP", "SLO", "RON", "EST",
    "LAV", "LIT",
];

/// Word characters per the TypeScript `wordCharRegex`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// The char starting at byte offset `byte`, if on a boundary.
fn char_at(s: &str, byte: usize) -> Option<char> {
    s.get(byte..).and_then(|rest| rest.chars().next())
}

/// Whether `c` is a single-separator char (space or dot).
fn is_sep(c: char) -> bool {
    c == ' ' || c == '.'
}

/// Whether the hyphen at byte `p` joins two non-separator neighbours.
fn is_tight_dash(w: &str, p: usize) -> bool {
    if char_at(w, p) != Some('-') || p == 0 {
        return false;
    }
    let prev = w[..p].chars().last();
    let next = w[p + 1..].chars().next();
    prev.is_some_and(|c| !is_sep(c)) && next.is_some_and(|c| !is_sep(c))
}

/// Strip punctuation from both edges of a token.
fn strip_punctuation(token: &str) -> String {
    js_regex::replace_all_str(surrounding_punctuation(), token, "")
}

/// Whether a token is a scene/subtitle/language stop word.
fn is_stop_word(token: &str) -> bool {
    let word = strip_punctuation(token);
    js_regex::is_match(episode_hard_stop(), &word)
        || js_regex::is_match(episode_language_word(), &word)
}

/// Whether a token is junk trimmed from the trailing edge.
fn is_trailing_tag(token: &str) -> bool {
    let word = strip_punctuation(token);
    js_regex::is_match(episode_trailing_tag(), &word) || LANGUAGE_CODES.iter().any(|c| *c == word)
}

/// Clean a raw title: strip junk tokens, markings, and alt-language titles.
///
/// Ports the TypeScript `cleanTitle`.
pub(crate) fn clean_title(raw_title: &str) -> String {
    let mut title = raw_title.trim().to_string();

    // Every step below only deletes text, so an absence found now still
    // holds for the later passes that key off non-ASCII characters.
    let non_ascii = title.chars().any(|c| u32::from(c) >= 128);

    if title.contains('_') {
        title = js_regex::replace_all_str(underscores(), &title, " ");
    }
    if title.contains('[') || title.contains('(') {
        title = js_regex::replace_all_str(movie_indicator(), &title, "");
    }
    title = js_regex::replace_all_str(not_allowed_symbols(), &title, "");

    // Clear Russian cast information.
    if (non_ascii || title.contains('/'))
        && let Some(caps) = russian_cast().captures(&title).ok().flatten()
    {
        let groups: Vec<String> = caps
            .iter()
            .skip(1)
            .flatten()
            .map(|g| g.as_str().to_string())
            .collect();
        for group in groups {
            title = title.replacen(&group, "", 1);
        }
    }

    if char_at(&title, 0).is_some_and(|c| "[【★".contains(c))
        && let Some(caps) = release_group_marking_start()
            .captures(&title)
            .ok()
            .flatten()
        && let Some(g) = caps.get(1)
    {
        title = g.as_str().to_string();
    }
    if title.chars().last().is_some_and(|c| "]】★".contains(c))
        && let Some(caps) = release_group_marking_end().captures(&title).ok().flatten()
        && let Some(g) = caps.get(1)
    {
        title = g.as_str().to_string();
    }

    if non_ascii {
        // Remove alternate-language titles.
        title = js_regex::replace_all_str(alt_titles(), &title, "");
        // Remove non-English chars when they are not the only ones left.
        if let Some(group) = not_only_non_english()
            .captures(&title)
            .ok()
            .flatten()
            .and_then(|caps| caps.iter().skip(1).flatten().next())
        {
            title = title.replacen(group.as_str(), "", 1);
        }
    }

    title = js_regex::replace_all_str(remaining_not_allowed(), &title, "");

    if !title.contains(' ') && title.contains('.') {
        title = js_regex::replace_all_str(dots_to_spaces(), &title, " ");
    }

    for (open, close) in [('{', '}'), ('[', ']'), ('(', ')')] {
        if title.chars().filter(|&c| c == open).count()
            != title.chars().filter(|&c| c == close).count()
        {
            title = title.replace([open, close], "");
        }
    }

    title = js_regex::replace_all_str(redundant_symbols_at_end(), &title, "");
    title = collapse_whitespace(&title);
    title.trim().to_string()
}

/// Collapse runs of whitespace to single spaces (without trimming edges).
pub(crate) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Extract the episode title from the parser's final working string.
///
/// `start` is the byte index just past the episode marker. Returns `None`
/// when the remaining text does not convincingly start an episode title.
///
/// Ports the TypeScript `extractEpisodeTitle`.
pub(crate) fn extract_episode_title(w: &str, start: usize, group: Option<&str>) -> Option<String> {
    let i = episode_title_start(w, start)?;
    let (mut tokens, terminator) = collect_episode_title_tokens(w, i);
    if matches!(terminator, ')' | ']' | '}') {
        return None;
    }
    trim_episode_title_debris(&mut tokens, group);
    plausible_episode_title(&tokens).then(|| tokens.join(" "))
}

/// The byte index where the episode title begins, when the junction between
/// the removed marker and the title is well-formed.
///
/// Exactly one separator may sit between the marker and the episode title;
/// two or more is a scar left by removed marker text.
fn episode_title_start(w: &str, start: usize) -> Option<usize> {
    let mut seps_before = 0;
    let mut i = start;
    while i > 0 {
        let Some(c) = char_at(w, i - 1) else { break };
        if !is_sep(c) {
            break;
        }
        seps_before += 1;
        i -= c.len_utf8();
    }
    // A tight hyphen delimiter leaves the title directly behind the
    // removed marker with no space or dot to mark the junction.
    if seps_before == 0 && start > 0 && is_tight_dash(w, start - 1) {
        seps_before = 1;
    }
    let mut seps_after = 0;
    let mut i = start;
    while let Some(c) = char_at(w, i) {
        if !is_sep(c) {
            break;
        }
        seps_after += 1;
        i += c.len_utf8();
    }
    (seps_before.min(1) + seps_after == 1 && i < w.len()).then_some(i)
}

/// Collect the raw tokens of the episode title starting at `i`.
///
/// Returns the tokens and the character that ended the scan, so callers can
/// reject bracket-terminated captures.
fn collect_episode_title_tokens(w: &str, i: usize) -> (Vec<String>, char) {
    let mut tokens: Vec<String> = Vec::new();
    let mut terminator = '\0';
    let mut i = i;
    while i < w.len() {
        // Sticky-match a token at exactly `i`.
        let token = episode_title_token()
            .captures_from_pos(w, i)
            .ok()
            .flatten()
            .and_then(|caps| caps.get(0).filter(|m| m.start() == i));
        let Some(token) = token else {
            terminator = char_at(w, i).unwrap_or('\0');
            break;
        };
        let text = token.as_str().to_string();
        // Punctuation-only tokens are list separators: skip leading ones
        // (marker debris), stop the capture on inner ones. "&" is a real
        // conjunction inside titles.
        if text != "&" && !text.chars().any(is_word_char) {
            if !tokens.is_empty() {
                break;
            }
        } else {
            tokens.push(text);
        }
        i += token.as_str().len();
        if let Some(c) = char_at(w, i)
            && is_sep(c)
            && char_at(w, i + c.len_utf8()).is_some_and(is_sep)
        {
            break; // scarce separator run ends the title
        }
        i += char_at(w, i).map_or(0, char::len_utf8);
    }
    (tokens, terminator)
}

/// Strip release-group / extension / version debris from the token edges.
fn trim_episode_title_debris(tokens: &mut Vec<String>, group: Option<&str>) {
    if let Some(first) = tokens.first_mut() {
        while first.starts_with('-') {
            first.remove(0);
        }
        if first.is_empty() {
            tokens.remove(0);
        }
    }
    while let Some(last) = tokens.last() {
        let mut trimmed = last.clone();
        if let Some(g) = group {
            let suffix = format!("-{}", g.to_lowercase());
            if trimmed.to_lowercase().ends_with(&suffix) {
                trimmed.truncate(trimmed.len() - suffix.len());
            }
        }
        let debris = trimmed.is_empty()
            || js_regex::is_match(extension_token(), &trimmed)
            || js_regex::is_match(version_token(), &trimmed)
            || group.is_some_and(|g| trimmed.to_lowercase() == g.to_lowercase())
            || is_stop_word(&trimmed)
            || is_trailing_tag(&trimmed);
        if debris {
            tokens.pop();
        } else {
            *tokens.last_mut().unwrap_or(&mut String::new()) = trimmed;
            break;
        }
    }
    // A tight-dash scheme can glue the last title word to a since-removed
    // tag ("Relations-1080p" -> "Relations-"); strip the trailing hyphens.
    if let Some(last) = tokens.last_mut() {
        while last.ends_with('-') {
            last.pop();
        }
        if last.is_empty() {
            tokens.pop();
        }
    }
}

/// Whether the collected tokens plausibly form an episode title.
fn plausible_episode_title(tokens: &[String]) -> bool {
    if tokens.is_empty() || tokens.len() > 12 {
        return false;
    }

    // Tokens carry their punctuation ("svensk,"), so compare bare words.
    let first_word = strip_punctuation(tokens.first().unwrap_or(&String::new()));

    // A scene/subtitle marker word never starts an episode title.
    if is_stop_word(&first_word) {
        return false;
    }
    // A leading language name is a language tag unless it clearly opens a
    // longer title: the next word must be a real title word.
    if js_regex::is_match(episode_language_word(), &first_word)
        && (tokens.len() <= 2
            || is_stop_word(tokens.get(1).unwrap_or(&String::new()))
            || !tokens
                .get(1)
                .is_some_and(|t| t.chars().any(char::is_lowercase)))
    {
        return false;
    }
    // A resolution-ish first token means leftover media info, not a title.
    if js_regex::is_match(resolution_ish(), &first_word) {
        return false;
    }
    // Single-word titles are held to a higher standard.
    if tokens.len() == 1
        && (first_word.chars().count() < 3 || !first_word.chars().any(char::is_lowercase))
    {
        return false;
    }

    let title = tokens.join(" ");
    js_regex::is_match(episode_letter(), &title)
        && !js_regex::is_match(of_remainder(), &title)
        && !js_regex::is_match(ep_remainder(), &title)
}

/// Byte-span positions for a match and each of its groups.
pub(crate) fn match_indices(caps: &Captures<'_, str>) -> MatchIndices {
    (0..caps.len())
        .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
        .collect()
}

/// The release group: the last token of the name or its leading `[Group]`.
///
/// Ports the TypeScript `extractGroup`. Returns the group text and its
/// byte index in the working string.
pub(crate) fn extract_group(w: &str, result: &ParseResult) -> Option<(String, usize)> {
    let claimed = |token: &str| -> bool {
        let lower = token.to_lowercase();
        for (field, meta) in result {
            if *field == "group" {
                continue;
            }
            let texts = &meta.matched;
            for i in 0..=texts.len() {
                let text: &str = if i < texts.len() {
                    &texts[i]
                } else {
                    &meta.m_value
                };
                if i > 0 && i == texts.len() && !texts.is_empty() && text == texts[texts.len() - 1]
                {
                    break;
                }
                if text.len() >= lower.len() && has_token(&text.to_lowercase(), &lower) {
                    return true;
                }
            }
        }
        false
    };
    let accept = |token: &str| !js_regex::is_match(episode_like(), token) && !claimed(token);
    // A removed tag's gap or a kept tag before the hyphen ends a tag block,
    // where a title word ("Spider-Man") does not.
    let follows_tag = |s: &str, hyphen: usize| -> bool {
        let before = &s[..hyphen];
        if js_regex::is_match(sep_end(), before) {
            return true;
        }
        let pm = last_token().captures(before).ok().flatten();
        pm.is_some_and(|caps| caps.get(1).is_some_and(|g| claimed(g.as_str())))
    };

    // A leading bracket that lost a tag to removal is a tag list, not a
    // group.
    let leading = before_title().captures(w).ok().flatten();
    let leading_group = leading.as_ref().and_then(|caps| {
        let g = caps.get(1)?.as_str();
        (!js_regex::is_match(leading_group_messy(), g)).then_some(g.to_string())
    });

    // A bare title has no tags, so its last hyphenated word is not a group.
    let any_field = result
        .keys()
        .any(|f| *f != "group" && *f != "container" && *f != "extension");
    let after_tags = leading_group.is_none()
        && ["resolution", "quality", "codec", "audio"]
            .iter()
            .any(|f| result.contains_key(f));

    // Separators at the end are left by tags removed after the group.
    let mut s = w.to_string();
    if let Some(m) = extension_with_lang().find(w).ok().flatten() {
        s.truncate(m.start());
    }
    let mut after_removal = false;
    loop {
        let mut end = s.len();
        while end > 0 {
            let Some(c) = s[..end].chars().last() else {
                break;
            };
            if c.is_whitespace() || c == '.' || c == '-' {
                end -= c.len_utf8();
            } else {
                break;
            }
        }
        if end < s.len() {
            s.truncate(end);
            after_removal = true;
        }
        let Some(bm) = trailing_bracket().captures(&s).ok().flatten() else {
            break;
        };
        let bracket_text = bm.get(1).map(|g| g.as_str()).unwrap_or_default();
        let gm = if after_tags && bracket_text.contains([' ', '.']) {
            bracket_group().captures(&s).ok().flatten()
        } else {
            None
        };
        if let Some(gm) = gm {
            let g1 = gm.get(1).map(|g| g.as_str()).unwrap_or_default();
            let gm_start = gm.get(0).map_or(0, |m| m.start());
            if follows_tag(&s, gm_start) && accept(g1) {
                let index = gm.get(0).map_or(0, |m| m.end()).saturating_sub(g1.len());
                return Some((g1.to_string(), index));
            }
        }
        s.truncate(bm.get(0).map_or(s.len(), |m| m.start()));
    }

    if any_field && let Some(tm) = trailing_group().captures(&s).ok().flatten() {
        let spaced = tm.get(1).is_some();
        let g2 = tm.get(2).map(|g| g.as_str()).unwrap_or_default();
        let tm_start = tm.get(0).map_or(0, |m| m.start());
        let ok = if spaced {
            after_tags && !after_removal
        } else {
            !after_removal || follows_tag(&s, tm_start)
        };
        if ok && accept(g2) {
            let index = tm.get(0).map_or(0, |m| m.end()).saturating_sub(g2.len());
            return Some((g2.to_string(), index));
        }
    }
    leading_group.map(|g| (g, 0))
}

/// Whether `text` contains `token` bounded by non-word characters.
fn has_token(text: &str, token: &str) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find(token) {
        let i = from + found;
        let end = i + token.len();
        let before_ok = i == 0 || !char_at(text, i - 1).is_some_and(is_word_char);
        let after_ok = end == text.len() || !char_at(text, end).is_some_and(is_word_char);
        if before_ok && after_ok {
            return true;
        }
        from = i + 1;
    }
    false
}

/// Value-set membership test used by processors.
pub(crate) fn value_set_contains(meta: &ParseMeta, item: &str) -> bool {
    matches!(&meta.value, Some(Value::Set(v)) if v.iter().any(|x| x == item))
}
