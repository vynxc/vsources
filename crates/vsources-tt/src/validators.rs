//! Match validators — extra checks beyond the raw pattern match.

use std::sync::OnceLock;

use crate::js_regex::{self, Regex};
use crate::types::MatchIndices;

/// Validators mirroring the TypeScript `validate_*` functions.
///
/// Each validator receives the working title and the byte spans of the
/// match (index `0`) plus every capture groups (indices `1..`).
#[derive(Debug, Clone)]
pub enum Validator {
    /// Succeeds when any inner validator succeeds.
    Or(Vec<Validator>),
    /// Succeeds when every inner validator succeeds.
    And(Vec<Validator>),
    /// Tests an anchored pattern against the text *before* the match.
    Lookbehind {
        /// Pattern compiled with `$` appended.
        re: Regex,
        /// `true` requires a match; `false` forbids it.
        polarity: bool,
    },
    /// Tests an anchored pattern against the text *after* the match.
    Lookahead {
        /// Pattern compiled with `^` prefixed.
        re: Regex,
        /// `true` requires a match; `false` forbids it.
        polarity: bool,
    },
    /// Requires the match to not start at the beginning of the title.
    NotAtStart,
    /// Requires the match to not end at the end of the title.
    NotAtEnd,
    /// When matching at the start, forbids whitespace ending the match.
    NotStartSpaced,
    /// Forbids the given pattern inside the matched text.
    NotMatch(Regex),
    /// Requires the given pattern inside the matched text.
    Match(Regex),
    /// Requires the given capture groups to hold the same text.
    MatchedGroupsAreSame(Vec<usize>),
}

impl Validator {
    /// Build a lookbehind validator from a JavaScript-style pattern.
    ///
    /// Mirrors the TypeScript `validateLookbehind(pattern, flags, polarity)`:
    /// the pattern is compiled with `$` appended. Invalid patterns degrade
    /// to a never-matching regex.
    #[must_use]
    pub fn lookbehind(pattern: &str, case_insensitive: bool, polarity: bool) -> Self {
        let anchored = format!("{pattern}$");
        let re = js_regex::compile(&anchored, case_insensitive).unwrap_or_else(never_regex);
        Self::Lookbehind { re, polarity }
    }

    /// Build a lookahead validator from a JavaScript-style pattern.
    ///
    /// Mirrors the TypeScript `validateLookahead(pattern, flags, polarity)`:
    /// the pattern is compiled with `^` prefixed. Invalid patterns degrade
    /// to a never-matching regex.
    #[must_use]
    pub fn lookahead(pattern: &str, case_insensitive: bool, polarity: bool) -> Self {
        let anchored = format!("^{pattern}");
        let re = js_regex::compile(&anchored, case_insensitive).unwrap_or_else(never_regex);
        Self::Lookahead { re, polarity }
    }

    /// Evaluate this validator against a match.
    pub(crate) fn check(&self, input: &str, idxs: &MatchIndices) -> bool {
        let whole = idxs.first().copied().flatten().unwrap_or((0, 0));
        let start = whole.0.min(input.len());
        let end = whole.1.min(input.len());
        match self {
            Self::Or(validators) => validators.iter().any(|v| v.check(input, idxs)),
            Self::And(validators) => validators.iter().all(|v| v.check(input, idxs)),
            Self::Lookbehind { re, polarity } => {
                let prefix = &input[..start];
                let matched = js_regex::is_match(re, prefix);
                if *polarity { matched } else { !matched }
            }
            Self::Lookahead { re, polarity } => {
                let suffix = &input[end..];
                let matched = js_regex::is_match(re, suffix);
                if *polarity { matched } else { !matched }
            }
            Self::NotAtStart => whole.0 != 0,
            Self::NotAtEnd => whole.1 != input.len(),
            Self::NotStartSpaced => {
                if whole.0 != 0 {
                    return true;
                }
                let matched = &input[start..end];
                !matched.chars().last().is_some_and(char::is_whitespace)
            }
            Self::NotMatch(re) => !js_regex::is_match(re, &input[start..end]),
            Self::Match(re) => js_regex::is_match(re, &input[start..end]),
            Self::MatchedGroupsAreSame(indices) => {
                let first = indices
                    .first()
                    .and_then(|i| idxs.get(*i).copied().flatten())
                    .map(|(s, e)| input[s.min(input.len())..e.min(input.len())].to_string());
                let Some(first) = first else {
                    return false;
                };
                indices.iter().all(|i| {
                    idxs.get(*i).copied().flatten().is_some_and(|(s, e)| {
                        input[s.min(input.len())..e.min(input.len())] == first
                    })
                })
            }
        }
    }
}

/// A shared regex that never matches, used when an invalid pattern is
/// supplied at handler-construction time so validation fails safely.
#[allow(clippy::expect_used)] // "(?!)" is a statically valid pattern.
fn never_regex() -> Regex {
    static NEVER: OnceLock<Regex> = OnceLock::new();
    NEVER
        .get_or_init(|| fancy_regex::Regex::new("(?!)").expect("(?!) always compiles"))
        .clone()
}
