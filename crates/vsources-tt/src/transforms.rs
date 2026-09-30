//! Value transformations applied after a handler matches.

use crate::js_regex;
use crate::types::{ParseMeta, ParseResult, Value};

/// Transformations mirroring the TypeScript `to_*` transformer functions.
#[derive(Debug, Clone)]
pub enum Transform {
    /// Set the value to a constant string.
    Value(String),
    /// Lowercase the string value.
    Lowercase,
    /// Uppercase the string value.
    Uppercase,
    /// Trim the string value.
    Trimmed,
    /// Strip `st|nd|rd|th` from day numbers.
    CleanDate,
    /// Abbreviate month names to three letters.
    CleanMonth,
    /// Normalize a date to `YYYY-MM-DD` for the given Go-style layout.
    Date(String),
    /// Parse a year or year range.
    Year,
    /// Expand a numeric range or sequence into a list.
    IntRange,
    /// Expand `1..N` markers into a full list.
    IntRangeTill,
    /// Parse a single integer into a one-element list.
    IntArray,
    /// Append a suffix to the string value.
    WithSuffix(String),
    /// Set the value to `true`.
    Boolean,
    /// Append a constant to the field's value set.
    ValueSet(String),
    /// Append the match text (upper- or lowercased) to the value set.
    ValueSetTransform(bool),
    /// Split the match text on non-alphanumerics and append each part
    /// uppercased to the value set.
    ValueSetMultiUppercase,
    /// Clean the date, abbreviate the month, then parse with the layout.
    DateCombo(String),
    /// Parse the year and set `complete` when a range is present.
    YearWithComplete,
    /// Parse episodes unless a season range already matched.
    EpisodeUnlessSeasonRange,
    /// Parse the Spanish `Cap.SSEE` composite episode marker.
    SpanishCapEpisode,
}

impl Transform {
    /// Apply this transformation in place.
    pub(crate) fn apply(&self, title: &str, meta: &mut ParseMeta, result: &mut ParseResult) {
        match self {
            Self::Value(v) => meta.value = Some(Value::Str(v.clone())),
            Self::Lowercase => set_str(meta, str::to_lowercase),
            Self::Uppercase => set_str(meta, str::to_uppercase),
            Self::Trimmed => set_str(meta, |s| s.trim().to_string()),
            Self::CleanDate => set_str(meta, clean_date),
            Self::CleanMonth => set_str(meta, clean_month),
            Self::Date(format) => set_str(meta, |s| parse_date(s, format).unwrap_or_default()),
            Self::Year => apply_year(meta),
            Self::IntRange => apply_int_range(meta),
            Self::IntRangeTill => apply_int_range_till(meta),
            Self::IntArray => apply_int_array(meta),
            Self::WithSuffix(suffix) => set_str(meta, |s| format!("{s}{suffix}")),
            Self::Boolean => meta.value = Some(Value::Bool(true)),
            Self::ValueSet(v) => append_value_set(meta, v.clone()),
            Self::ValueSetTransform(to_upper) => {
                let v = if *to_upper {
                    meta.m_value.to_uppercase()
                } else {
                    meta.m_value.to_lowercase()
                };
                append_value_set(meta, v);
            }
            Self::ValueSetMultiUppercase => {
                let parts: Vec<String> = meta
                    .m_value
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|p| !p.is_empty())
                    .map(str::to_uppercase)
                    .collect();
                for part in parts {
                    append_value_set(meta, part);
                }
            }
            Self::DateCombo(format) => {
                set_str(meta, clean_date);
                set_str(meta, clean_month);
                set_str(meta, |s| parse_date(s, format).unwrap_or_default());
            }
            Self::YearWithComplete => {
                apply_year(meta);
                if !result.contains_key("complete")
                    && let Some(Value::Str(v)) = &meta.value
                    && v.contains('-')
                {
                    let _ = v;
                    result.insert(
                        "complete",
                        ParseMeta {
                            m_index: meta.m_index,
                            m_value: meta.m_value.clone(),
                            matched: vec![meta.m_value.clone()],
                            value: Some(Value::Bool(true)),
                            remove: false,
                            processed: false,
                        },
                    );
                }
            }
            Self::EpisodeUnlessSeasonRange => {
                if let Some(seasons) = result.get("seasons")
                    && matches!(&seasons.value, Some(Value::Ints(v)) if v.len() > 1)
                {
                    meta.value = None;
                    return;
                }
                apply_int_array(meta);
            }
            Self::SpanishCapEpisode => apply_spanish_cap(meta, result, title),
        }
    }
}

/// Helper to mutate a string value, leaving other variants untouched.
fn set_str(meta: &mut ParseMeta, f: impl FnOnce(&str) -> String) {
    if let Some(Value::Str(s)) = &meta.value {
        meta.value = Some(Value::Str(f(s)));
    }
}

/// Strip `st|nd|rd|th` suffixes from day numbers.
fn clean_date(s: &str) -> String {
    let re = js_regex::compile_ci(r"(\d+)(?:st|nd|rd|th)").filter(|r| js_regex::is_match(r, s));
    match re {
        Some(re) => js_regex::replace_all(&re, s, &|_, groups| {
            groups.first().cloned().flatten().unwrap_or_default()
        }),
        None => s.to_string(),
    }
}

/// Abbreviate month names to their first three letters.
fn clean_month(s: &str) -> String {
    const MONTHS: &str = r"feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?";
    let pattern = format!("(?:{MONTHS})");
    let Some(re) = js_regex::compile_ci(&pattern) else {
        return s.to_string();
    };
    js_regex::replace_all(&re, s, &|matched, _| {
        matched.chars().take(3).collect::<String>()
    })
}

/// Parse a date string against a Go-style layout, producing `YYYY-MM-DD`.
fn parse_date(date_str: &str, format: &str) -> Option<String> {
    let normalized: String = date_str
        .chars()
        .map(|c| {
            matches!(c, '.' | '-' | '/' | '\\')
                .then_some(' ')
                .unwrap_or(c)
        })
        .collect();
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    let format_parts: Vec<&str> = format.split_whitespace().collect();

    let mut year: i64 = 0;
    let mut month: i64 = 0;
    let mut day: i64 = 0;

    for (i, fmt) in format_parts.iter().enumerate() {
        let val = parts.get(i).copied()?;
        match *fmt {
            "2006" | "YYYY" => year = val.parse().ok()?,
            "06" | "YY" => {
                year = 2000 + val.parse::<i64>().ok()?;
                if year > 2069 {
                    year -= 100;
                }
            }
            "01" | "MM" => month = val.parse().ok()?,
            "02" | "DD" | "_2" => day = val.parse().ok()?,
            "Jan" | "MMM" => month = month_index(val)?,
            "20060102" | "YYYYMMDD" => {
                if val.len() < 8 {
                    return None;
                }
                year = val.get(..4)?.parse().ok()?;
                month = val.get(4..6)?.parse().ok()?;
                day = val.get(6..8)?.parse().ok()?;
            }
            _ => {}
        }
    }
    if (1..=12).contains(&month) && (1..=31).contains(&day) && year > 0 {
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    }
    None
}

/// Month name to number.
fn month_index(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let prefix: String = name.to_lowercase().chars().take(3).collect();
    MONTHS
        .iter()
        .position(|m| *m == prefix)
        .and_then(|i| i64::try_from(i + 1).ok())
}

/// Year / year-range parsing from a possibly-composite value.
fn apply_year(meta: &mut ParseMeta) {
    let Some(Value::Str(v)) = &meta.value else {
        return;
    };
    let v = v.clone();
    let parts: Vec<&str> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() == 1 {
        meta.value = Some(Value::Str(parts[0].to_string()));
        return;
    }
    let start = parts[0].to_string();
    let end = parts[1].to_string();
    let Some(mut end_year) = end.parse::<i64>().ok() else {
        meta.value = Some(Value::Str(start));
        return;
    };
    let Some(start_year) = start.parse::<i64>().ok() else {
        meta.value = Some(Value::Str(String::new()));
        return;
    };
    if (0..100).contains(&end_year) {
        end_year += start_year - start_year % 100;
    }
    if end_year <= start_year {
        meta.value = Some(Value::Str(String::new()));
        return;
    }
    meta.value = Some(Value::Str(format!("{start_year}-{end_year}")));
}

/// Range/sequence expansion for numeric lists.
fn apply_int_range(meta: &mut ParseMeta) {
    let Some(Value::Str(v)) = &meta.value else {
        meta.value = None;
        return;
    };
    let v = v.clone();
    let parts: Vec<&str> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .collect();
    let nums: Vec<i64> = parts.iter().filter_map(|p| p.parse().ok()).collect();

    if nums.len() == 2 && nums[0] < nums[1] {
        meta.value = Some(Value::Ints((nums[0]..=nums[1]).collect()));
        return;
    }
    for pair in nums.windows(2) {
        if pair[0] + 1 != pair[1] {
            meta.value = None;
            return;
        }
    }
    meta.value = Some(Value::Ints(nums));
}

/// `1..N` expansion.
fn apply_int_range_till(meta: &mut ParseMeta) {
    let Some(Value::Str(v)) = &meta.value else {
        meta.value = None;
        return;
    };
    let v = v.clone();
    let parts: Vec<&str> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .collect();
    let Some(first) = parts.first().and_then(|p| p.parse::<i64>().ok()) else {
        meta.value = None;
        return;
    };
    meta.value = Some(Value::Ints((1..=first).collect()));
}

/// Single-integer list.
fn apply_int_array(meta: &mut ParseMeta) {
    let value = match &meta.value {
        Some(Value::Str(s)) => s.trim().parse::<i64>().ok().map(|n| vec![n]),
        _ => None,
    };
    meta.value = Some(Value::Ints(value.unwrap_or_default()));
}

/// Append to a value-set field.
fn append_value_set(meta: &mut ParseMeta, v: String) {
    if let Some(Value::Set(set)) = &mut meta.value
        && !set.contains(&v)
    {
        set.push(v);
    }
}

/// The Spanish `Cap.SSEE` composite marker transform.
fn apply_spanish_cap(meta: &mut ParseMeta, result: &ParseResult, _title: &str) {
    let Some(re) = js_regex::compile_ci(r"(\d{1,2})(\d{2})(?:[ _-](\d{1,2})(\d{2}))?\b\s*$") else {
        meta.value = None;
        return;
    };
    let Some(caps) = re.captures(&meta.m_value).ok().flatten() else {
        meta.value = None;
        return;
    };
    let group = |i: usize| caps.get(i).map(|m| m.as_str().to_string());
    let g1 = group(1);
    let g2 = group(2);
    let g3 = group(3);
    let g4 = group(4);
    let (Some(season_str), Some(ep_start_str)) = (g1.clone(), g2.clone()) else {
        meta.value = None;
        return;
    };
    let Some(season) = season_str.parse::<i64>().ok() else {
        meta.value = None;
        return;
    };
    let Some(ep_start) = ep_start_str.parse::<i64>().ok() else {
        meta.value = None;
        return;
    };
    let ep_end = g4.as_deref().and_then(|s| s.parse::<i64>().ok());

    let parsed_season = result
        .get("seasons")
        .and_then(|seasons| match &seasons.value {
            Some(Value::Ints(v)) if v.len() == 1 => Some(v[0]),
            _ => None,
        });
    if let Some(ps) = parsed_season
        && ps != season
    {
        // The Cap. prefix disagrees with the explicit season, so the
        // whole number is the episode (e.g. "Temporada 1 ... Cap.849").
        let whole1: i64 = format!("{season_str}{ep_start_str}")
            .parse()
            .ok()
            .unwrap_or_default();
        let whole2: Option<i64> = match (&g3, &g4) {
            (Some(a), Some(b)) => format!("{a}{b}").parse().ok(),
            _ => None,
        };
        if let Some(w2) = whole2
            && w2 > whole1
        {
            meta.value = Some(Value::Ints((whole1..=w2).collect()));
            return;
        }
        meta.value = Some(Value::Ints(vec![whole1]));
        return;
    }
    if let Some(ep_end) = ep_end
        && ep_end > ep_start
    {
        meta.value = Some(Value::Ints((ep_start..=ep_end).collect()));
        return;
    }
    meta.value = Some(Value::Ints(vec![ep_start]));
}
