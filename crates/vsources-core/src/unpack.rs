//! Dean Edwards `p.a.c.k.e.d` JavaScript unpacker.
//!
//! Ports the `unpacker` npm package (as used by `src/utils/embed.js`) so
//! obfuscated embed pages can be unpacked without a JavaScript runtime.
//!
//! A packed block looks like
//! `eval(function(p,a,c,k,e,d){…}('payload', 62, 4, 'key1|key2|…'.split('|'), 0, {}))`.
//! Every word token in the payload is interpreted as a number in the given
//! radix; when that number indexes a non-empty key, the token is replaced
//! by that key, and otherwise the token is left untouched.

use fancy_regex::Regex;
use url::Url;

/// Marker that starts a packed `eval(function(p,a,c,k,e,d){…})` block.
const EVAL_MARKER: &str = "eval(function(p,a,c,k,e,d";
/// The packer call's argument list begins right after `}('`.
const ARGS_MARKER: &str = "}(\'";
/// Keys are always passed as `'k1|k2|…'.split('|')`.
const SPLIT_CALL: &str = ".split('|')";
/// Extended alphabet for bases 37–62 (the upstream `Unbaser` definition).
const ALPHABET_62: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// Alphabet for base 95: printable ASCII 32–126, in order.
///
/// The upstream package's literal carries a stray leading quote that shifts
/// every digit value; this is the corrected form of the same table.
const ALPHABET_95: &str = " !\"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

/// Detect whether `source` is P.A.C.K.E.R. packed code.
///
/// Mirrors the upstream `detect`: one space is removed (tolerating a stray
/// space after `eval`) and the result must start with the packer header.
/// This is a cheap prefix check; use [`unpack_eval`] for full validation.
#[must_use]
pub fn detect(source: &str) -> bool {
    source
        .replacen(' ', "", 1)
        .starts_with("eval(function(p,a,c,k,e,")
}

/// Unpack the first `eval(function(p,a,c,k,e,d){…}(…))` block in `html`.
///
/// Returns the unpacked JavaScript source, or `None` when no well-formed
/// packed block is present (the upstream package throws on malformed
/// input; `None` plays that role here).
#[must_use]
pub fn unpack_eval(html: &str) -> Option<String> {
    let start = html.find(EVAL_MARKER)?;
    let after_marker = &html[start..];
    // The argument list starts at `}('` — the end of the packer function
    // body followed by the payload literal. That sequence cannot occur
    // inside the body itself, so the first occurrence is the real one.
    let args = after_marker.find(ARGS_MARKER)?;
    // Land on the payload's opening quote.
    let payload_lit = &after_marker[args + ARGS_MARKER.len() - 1..];
    let (payload, rest) = split_raw_string_literal(payload_lit)?;

    // Argument list: payload, radix, count, keys. Commas may be followed
    // by optional spaces, mirroring the upstream juicer regexes.
    let rest = after_separator(rest)?;
    let (radix_str, rest) = split_digits(rest)?;
    let rest = after_separator(rest)?;
    let (count_str, rest) = split_digits(rest)?;
    let rest = after_separator(rest)?;
    let (keys_raw, rest) = split_raw_string_literal(rest)?;
    // Both upstream juicer regexes require the `.split('|')` call.
    if !rest.trim_start().starts_with(SPLIT_CALL) {
        return None;
    }

    let radix: u32 = radix_str.parse().ok()?;
    let count: usize = count_str.parse().ok()?;
    let keys: Vec<&str> = keys_raw.split('|').collect();
    // A symtab length that disagrees with the declared count is malformed.
    if count != keys.len() {
        return None;
    }

    substitute_keys(payload, radix, &keys)
}

/// Split a raw single-quoted JavaScript string literal off the front of `s`.
///
/// `s` must start with `'`. Escapes are left untouched — the upstream
/// unpacker substitutes tokens on the raw payload, so `\'` sequences stay
/// in the output. Returns the raw literal content and the text after the
/// closing quote.
fn split_raw_string_literal(s: &str) -> Option<(&str, &str)> {
    let mut iter = s.char_indices();
    // Skip the opening quote.
    iter.next();
    while let Some((i, c)) = iter.next() {
        match c {
            '\'' => return Some((&s[1..i], &s[i + 1..])),
            // Skip the escaped character.
            '\\' => {
                iter.next();
            }
            _ => {}
        }
    }
    None
}

/// Skip an argument separator: optional spaces, a comma, more spaces.
fn after_separator(rest: &str) -> Option<&str> {
    rest.trim_start().strip_prefix(',').map(str::trim_start)
}

/// Split leading ASCII digits off the front of `s`.
fn split_digits(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Replace every word token in the raw payload with its symtab entry.
///
/// A token is replaced only when it decodes, in the given radix, to a
/// symtab index holding a non-empty key — exactly the upstream `lookup`
/// (`word2 || word`), so unknown words survive verbatim.
fn substitute_keys(payload: &str, radix: u32, keys: &[&str]) -> Option<String> {
    let unbase = Unbase::new(radix)?;
    let mut out = String::with_capacity(payload.len());
    let mut word = String::new();
    for c in payload.chars() {
        if is_word_char(c) {
            word.push(c);
        } else {
            flush_word(&mut out, &word, &unbase, keys);
            word.clear();
            out.push(c);
        }
    }
    flush_word(&mut out, &word, &unbase, keys);
    Some(out)
}

/// Append `word` to `out`, replaced by its symtab entry when applicable.
fn flush_word(out: &mut String, word: &str, unbase: &Unbase, keys: &[&str]) {
    if let Some(key) = unbase
        .unbase(word)
        .and_then(|index| keys.get(index))
        .filter(|key| !key.is_empty())
    {
        out.push_str(key);
    } else {
        out.push_str(word);
    }
}

/// JavaScript `\w` (ASCII only, like the upstream regex).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Radix decoder mirroring the upstream `Unbaser`.
struct Unbase {
    /// How this base decodes words.
    strategy: Strategy,
}

/// The two decoding modes the upstream package uses.
enum Strategy {
    /// Bases up to 36, with `parseInt`-style lenient decoding: the longest
    /// valid prefix, and no match when there is no valid leading digit.
    Small(u32),
    /// Bases 37–95, with a fixed alphabet and strict position arithmetic.
    Alphabet(&'static str),
}

impl Unbase {
    /// Build a decoder for `radix`.
    ///
    /// Base 1 is treated as decimal (the upstream package errors on it,
    /// but decimal is the only sensible meaning for a one-symbol radix).
    fn new(radix: u32) -> Option<Self> {
        let strategy = match radix {
            1 => Strategy::Small(10),
            2..=36 => Strategy::Small(radix),
            37..=62 => Strategy::Alphabet(&ALPHABET_62[..radix as usize]),
            95 => Strategy::Alphabet(ALPHABET_95),
            _ => return None,
        };
        Some(Self { strategy })
    }

    /// Decode `word` to a symtab index; `None` when it does not decode.
    fn unbase(&self, word: &str) -> Option<usize> {
        match &self.strategy {
            Strategy::Small(radix) => {
                let digits = longest_radix_prefix(word, *radix)?;
                usize::from_str_radix(digits, *radix).ok()
            }
            Strategy::Alphabet(alphabet) => {
                let base = alphabet.len();
                let mut value: usize = 0;
                for c in word.chars() {
                    // Alphabets are pure ASCII, so byte offsets are values.
                    let digit = alphabet.find(c)?;
                    value = value.checked_mul(base).and_then(|v| v.checked_add(digit))?;
                }
                Some(value)
            }
        }
    }
}

/// The longest prefix of `word` whose characters are valid `radix` digits.
fn longest_radix_prefix(word: &str, radix: u32) -> Option<&str> {
    let mut end = 0;
    for (i, c) in word.char_indices() {
        match digit_value(c) {
            Some(value) if value < radix => end = i + c.len_utf8(),
            _ => break,
        }
    }
    (end > 0).then(|| &word[..end])
}

/// The value of `c` as a base-36 digit, case-insensitive like `parseInt`.
fn digit_value(c: char) -> Option<u32> {
    match c {
        '0'..='9' => Some(u32::from(c as u8 - b'0')),
        'a'..='z' | 'A'..='Z' => Some(u32::from(c.to_ascii_lowercase() as u8 - b'a') + 10),
        _ => None,
    }
}

/// Find the first capture group of `pattern` in unpacked embed HTML and
/// normalize it into an `https://` URL.
///
/// Ports `extractUrlFromPacked`: the first pattern whose capture group 1
/// matches wins, and a leading `https://` or `//` is stripped before the
/// URL is rebuilt over `https://`.
#[must_use]
pub fn extract_url_from_packed(html: &str, patterns: &[Regex]) -> Option<Url> {
    let unpacked = unpack_eval(html)?;
    for pattern in patterns {
        if let Ok(Some(caps)) = pattern.captures(&unpacked)
            && let Some(group) = caps.get(1)
        {
            let raw = group.as_str();
            let cleaned = raw
                .strip_prefix("https://")
                .or(raw.strip_prefix("//"))
                .unwrap_or(raw);
            return Url::parse(&format!("https://{cleaned}")).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upstream package's README example (base64-decoded).
    const README_PACKED: &str = r"eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('6 5=1;4 0(){3.2(\'0\')}',7,7,'test||log|console|function|testDeclaration|var'.split('|'),0,{}))";
    #[test]
    fn detects_packed_code() {
        assert!(detect(README_PACKED));
        assert!(!detect("var plain = 1;"));
        assert!(detect(
            "eval (function(p,a,c,k,e,d){}('x',10,1,'y'.split('|'),0,{}))"
        ));
    }

    #[test]
    fn unpacks_the_readme_example() {
        // `\'` escapes survive because substitution works on the raw
        // payload, exactly like the upstream package.
        assert_eq!(
            unpack_eval(README_PACKED).as_deref(),
            Some(r"var testDeclaration=1;function test(){console.log(\'test\')}")
        );
    }

    #[test]
    fn unpacks_a_base62_block() {
        // Radix 62, count 3: `0`/`1`/`2` decode to their keys; `10`
        // decodes to 62, which is out of range and survives verbatim.
        let packed = concat!(
            "eval(function(p,a,c,k,e,d){e=function(c){return c.toString(36)}};",
            "while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p",
            "}('0 1 2 10',62,3,'alpha|beta|gamma'.split('|'),0,{}))"
        );
        assert_eq!(unpack_eval(packed).as_deref(), Some("alpha beta gamma 10"));
    }

    #[test]
    fn keeps_undecodable_words() {
        let packed = concat!(
            "eval(function(p,a,c,k,e,d){e=function(c){return c}};",
            "while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p",
            "}('hello 0 world',10,2,'first|'.split('|'),0,{}))"
        );
        // `hello`/`world` do not decode; `0` → `first`; index 1 has an
        // empty key so a `1` token would survive too.
        assert_eq!(unpack_eval(packed).as_deref(), Some("hello first world"));
    }

    #[test]
    fn rejects_malformed_blocks() {
        // No packed marker.
        assert_eq!(unpack_eval("nothing here"), None);
        // Symtab length disagrees with the declared count.
        let bad = concat!(
            "eval(function(p,a,c,k,e,d){e=function(c){return c}};",
            "}('a b',10,3,'x|y'.split('|'),0,{}))"
        );
        assert_eq!(unpack_eval(bad), None);
        // Radix `[]` is not a number.
        let bracket = concat!(
            "eval(function(p,a,c,k,e,d){e=function(c){return c}};",
            "}('a b',[],3,'x|y|z'.split('|'),0,{}))"
        );
        assert_eq!(unpack_eval(bracket), None);
    }

    #[test]
    fn extracts_a_url_from_packed_html() {
        let packed = concat!(
            "eval(function(p,a,c,k,e,d){e=function(c){return c}};",
            "}('var u=\"0://1.example/file.mp4\";',16,2,'https|cdn'.split('|'),0,{}))"
        );
        let Some(pattern) = Regex::new(r#""(https?://[^"]+)""#).ok() else {
            panic!("regex literal must compile");
        };
        let url = extract_url_from_packed(packed, std::slice::from_ref(&pattern))
            .unwrap_or_else(|| panic!("expected a URL to be extracted"));
        assert_eq!(url.as_str(), "https://cdn.example/file.mp4");
    }
}
