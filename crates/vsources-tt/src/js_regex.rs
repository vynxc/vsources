//! JavaScript-style regex translation for `fancy-regex`.
//!
//! Handler patterns are ported from TypeScript sources with their pattern
//! text kept verbatim. JavaScript regex semantics differ from Rust's in a
//! few important ways; this module translates the pattern text so that
//! [`Regex`] behaves like the original:
//!
//! - `\d`, `\w`, `\s`, `\b` are ASCII-only in JavaScript but Unicode-aware
//!   in Rust, so they are rewritten to explicit ASCII classes.
//! - An unescaped `]` outside a character class is legal (Annex B) in
//!   JavaScript without the `u` flag; it is escaped here.
//! - `\uXXXX` escapes are rewritten to the Rust `\u{XXXX}` form.

use std::fmt::Write as _;

/// A compiled JavaScript-style regular expression.
pub(crate) type Regex = fancy_regex::Regex;

/// JavaScript `\s` members: the ASCII whitespace set — including the
/// plain space U+0020, without which `\s` never matches a space — plus
/// the Unicode spaces ECMAScript defines.
const JS_SPACE_MEMBERS: &str = r"\x20\t\n\x0B\f\r\u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
/// JavaScript `\w` members (ASCII only).
const JS_WORD_MEMBERS: &str = "0-9A-Za-z_";
/// JavaScript's `\b`: a boundary between an ASCII word character and
/// anything else (all non-ASCII characters are non-word in JS).
const JS_WORD_BOUNDARY: &str =
    r"(?:(?<![0-9A-Za-z_])(?=[0-9A-Za-z_])|(?<=[0-9A-Za-z_])(?![0-9A-Za-z_]))";
/// JavaScript's `\B`: the complement of [`JS_WORD_BOUNDARY`].
const JS_NOT_WORD_BOUNDARY: &str =
    r"(?:(?<![0-9A-Za-z_])(?![0-9A-Za-z_])|(?<=[0-9A-Za-z_])(?=[0-9A-Za-z_]))";

/// Compile a JavaScript-style pattern with the `i` flag.
pub(crate) fn compile_ci(pattern: &str) -> Option<Regex> {
    compile(pattern, true)
}

/// A regex that never matches; fallback for invalid static patterns.
#[allow(clippy::expect_used)] // "(?!) " is a statically valid pattern.
pub(crate) fn never_match() -> Regex {
    fancy_regex::Regex::new("(?!)").expect("(?!) always compiles")
}

/// Compile a JavaScript-style pattern, translating it to Rust regex syntax.
///
/// Returns `None` (and logs) when the translated pattern fails to compile —
/// the handler is then skipped instead of panicking.
pub(crate) fn compile(pattern: &str, case_insensitive: bool) -> Option<Regex> {
    let translated = translate(pattern);
    match fancy_regex::RegexBuilder::new(&translated)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(regex) => Some(regex),
        Err(err) => {
            crate::log_invalid_pattern(pattern, &err);
            None
        }
    }
}

/// Is this character a hex digit?
fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Translate a JavaScript regex body into `fancy-regex` syntax.
fn translate(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    // Whether we are inside a character class.
    let mut in_class = false;

    while let Some(c) = chars.next() {
        match c {
            '[' if !in_class => {
                in_class = true;
                out.push('[');
            }
            // A literal `[` inside a class is legal in JavaScript but must
            // be escaped for the Rust regex parser.
            '[' if in_class => out.push_str(r"\["),
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            ']' => {
                // Annex B: literal `]` outside a class.
                out.push_str(r"\]");
            }
            '\\' => translate_escape(&mut out, &mut chars, in_class),
            other => out.push(other),
        }
    }
    out
}

/// Translate the escape sequence following a backslash.
fn translate_escape(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    in_class: bool,
) {
    match chars.next() {
        // Positive shorthands inside a class contribute their members.
        Some('d') if in_class => out.push_str("0-9"),
        Some('w') if in_class => out.push_str(JS_WORD_MEMBERS),
        Some('s') if in_class => out.push_str(JS_SPACE_MEMBERS),
        // Negated shorthands inside a class are nested classes: Rust regex
        // unions adjacent/nested classes, so `[.\W]` becomes
        // `[.[^0-9A-Za-z_]]` — the JavaScript meaning, not the inverse.
        Some('D') if in_class => out.push_str("[^0-9]"),
        Some('W') if in_class => out.push_str("[^0-9A-Za-z_]"),
        Some('S') if in_class => {
            out.push_str("[^");
            out.push_str(JS_SPACE_MEMBERS);
            out.push(']');
        }
        Some('d') => out.push_str("[0-9]"),
        Some('D') => out.push_str("[^0-9]"),
        Some('w') => out.push_str("[0-9A-Za-z_]"),
        Some('W') => out.push_str("[^0-9A-Za-z_]"),
        Some('s') => {
            out.push('[');
            out.push_str(JS_SPACE_MEMBERS);
            out.push(']');
        }
        Some('S') => {
            out.push_str("[^");
            out.push_str(JS_SPACE_MEMBERS);
            out.push(']');
        }
        // Word boundary. JavaScript's `\b` is ASCII-only while Rust's is
        // Unicode-aware: "Árabe" starts with a word character for Rust but
        // not for JS, so a raw `\b` would invent boundaries around
        // non-ASCII words that the upstream parser cannot see. Expanding
        // to explicit ASCII lookarounds reproduces JS exactly.
        Some('b') if in_class => out.push_str("\\x08"),
        Some('b') => out.push_str(JS_WORD_BOUNDARY),
        Some('B') => out.push_str(JS_NOT_WORD_BOUNDARY),
        Some('u') => {
            // `\uXXXX` or `\u{X...}` → Rust `\u{...}`.
            if chars.peek() == Some(&'{') {
                chars.next();
                let mut hex = String::new();
                while let Some(&h) = chars.peek() {
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                    chars.next();
                }
                chars.next(); // consume '}'
                let _ = write!(out, "\\u{{{hex}}}");
            } else {
                let mut hex = String::new();
                for _ in 0..4 {
                    match chars.peek() {
                        Some(&h) if is_hex(h) => {
                            hex.push(h);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if hex.is_empty() {
                    out.push_str(r"\u");
                } else {
                    let _ = write!(out, "\\u{{{hex}}}");
                }
            }
        }
        Some('x') => {
            // `\xHH` is valid in both dialects.
            out.push_str(r"\x");
        }
        Some(p @ ('p' | 'P')) => {
            // Unicode property escapes: pass through (fancy-regex supports
            // them; JavaScript requires the `u` flag).
            out.push('\\');
            out.push(p);
        }
        Some(other) => {
            out.push('\\');
            out.push(other);
        }
        None => out.push('\\'),
    }
}

/// Replace every match of `regex` in `text` with the string produced by
/// `replacement`, mirroring JavaScript `String.replace` with a global regex.
///
/// The replacement callback receives the whole-match text and the capture
/// group texts (with `None` for non-participating groups).
pub(crate) fn replace_all(
    regex: &Regex,
    text: &str,
    replacement: &dyn Fn(&str, &[Option<String>]) -> String,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut search_from = 0;
    while let Some(caps) = regex.captures_from_pos(text, search_from).ok().flatten() {
        let Some(m) = caps.get(0) else { break };
        out.push_str(&text[last_end..m.start()]);
        let groups: Vec<Option<String>> = (1..caps.len())
            .map(|i| caps.get(i).map(|g| g.as_str().to_string()))
            .collect();
        out.push_str(&replacement(m.as_str(), &groups));
        last_end = m.end();
        search_from = if m.start() == m.end() {
            m.end() + 1
        } else {
            m.end()
        };
    }
    out.push_str(&text[last_end.min(text.len())..]);
    out
}

/// Replace every non-overlapping match with a plain string.
pub(crate) fn replace_all_str(regex: &Regex, text: &str, replacement: &str) -> String {
    replace_all(regex, text, &|_, _| replacement.to_string())
}

/// Test whether `regex` matches anywhere in `text`.
pub(crate) fn is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_s_matches_plain_space() {
        let re = compile_ci(r"5[.\s]1").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "5 1"));
        assert!(is_match(&re, "5.1"));
        assert!(!is_match(&re, "5x1"));
        let re = compile_ci(r"\s").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "a b"));
        assert!(!is_match(&re, "ab"));
    }

    #[test]
    fn negated_shorthands_keep_their_meaning_inside_classes() {
        // `5[\W]1` must match separators, not word characters.
        let re = compile_ci(r"5[\W]1").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "5 1"));
        assert!(is_match(&re, "5.1"));
        assert!(!is_match(&re, "5x1"));
        // `[\Wex]` unions the members with the negated shorthand.
        let re = compile_ci(r"\bfoo[\Wex]bar\b").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "foo bar"));
        assert!(is_match(&re, "fooxbar"));
        assert!(is_match(&re, "fooebar"));
        assert!(!is_match(&re, "fooabar"));
    }

    #[test]
    fn negated_space_outside_class_is_the_full_negation() {
        let re = compile_ci(r"\S").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "x"));
        assert!(!is_match(&re, " "));
        assert!(!is_match(&re, "\u{a0}"));
    }

    #[test]
    fn word_boundaries_are_ascii_only_like_javascript() {
        // The upstream corpus case: JavaScript's `\b` never fires between
        // two non-ASCII letters, so "Árabe" must not match `\barabe\b`
        // even with the accented alternative in the pattern.
        let re = compile_ci(r"\b(?:arabic|[aá]rabe|ara)\b")
            .unwrap_or_else(|| panic!("pattern must compile"));
        assert!(!is_match(&re, "Inglês,Português,Espanhol,Árabe,Tailandês"));
        assert!(is_match(&re, "arabe"));
        assert!(is_match(&re, "ara dub"));
        // A boundary must still fire between ASCII word and non-word
        // characters, including at string edges.
        let re = compile_ci(r"\beng\b").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "eng"));
        assert!(is_match(&re, ".eng."));
        assert!(!is_match(&re, "english"));
        assert!(!is_match(&re, "weng"));
        // `\B` is the complement.
        let re = compile_ci(r"ar\Barabe").unwrap_or_else(|| panic!("pattern must compile"));
        assert!(is_match(&re, "lararabe"));
        assert!(!is_match(&re, "ar arabe"));
    }
}
