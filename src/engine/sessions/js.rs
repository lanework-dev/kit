//! JavaScript's string rules, where the app's search depends on them: what
//! `\s` and `trim()` take for whitespace, `length` counting UTF-16 units,
//! `RegExp`'s `iu` case folding, and which values are truthy.
//!
//! NOTE: Snippets and their ranges are cut and counted in UTF-16 units, as
//! `search.ts` does, so the offsets `lwk` answers are the app's.

use serde_json::Value;

/// Whether `c` is whitespace to JavaScript: `\s`, and what `trim()` strips.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{D}'
            | ' '
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn is_space_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(is_space)
}

/// `text.trim()`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `text.trimStart()`.
pub fn trim_start(text: &str) -> &str {
    text.trim_start_matches(is_space)
}

/// The UTF-16 units of `text`: what JavaScript indexes.
pub fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Text from units; a lone surrogate becomes U+FFFD.
pub fn from_utf16(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// `units.replace(/\s+/g, " ")`.
pub fn collapse_space(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    let mut in_space = false;
    for &unit in units {
        if is_space_unit(unit) {
            if !in_space {
                out.push(u16::from(b' '));
            }
            in_space = true;
        } else {
            out.push(unit);
            in_space = false;
        }
    }
    out
}

/// `units.trim()`.
pub fn trim_units(units: &[u16]) -> &[u16] {
    let start = units
        .iter()
        .position(|&unit| !is_space_unit(unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|&unit| !is_space_unit(unit))
        .map_or(start, |at| at + 1);
    &units[start..end]
}

/// JavaScript's truthiness of a parsed JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Whether `key` is an array index to JavaScript, which lists those keys of
/// an object first, in numeric order.
pub fn index_key(key: &str) -> Option<u32> {
    if key.is_empty()
        || !key.bytes().all(|b| b.is_ascii_digit())
        || (key.len() > 1 && key.starts_with('0'))
    {
        return None;
    }
    key.parse::<u32>().ok().filter(|index| *index != u32::MAX)
}

/// The simple case fold of one UTF-16 unit, as a `RegExp` with the `iu`
/// flags matches: `K` (Kelvin) is `k`, `ς` is `σ`; a surrogate is itself.
pub fn fold(unit: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    if c.is_ascii() {
        return u16::from(c.to_ascii_lowercase() as u8);
    }
    // Turkish dotless i has no simple fold.
    if c == '\u{131}' {
        return unit;
    }
    let up = one(c.to_uppercase()).unwrap_or(c);
    let low = one(up.to_lowercase()).unwrap_or(up);
    u16::try_from(u32::from(low)).unwrap_or(unit)
}

fn one(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The query as a literal, found without regard to case.
///
/// Text is searched in UTF-16 units so that offsets are JavaScript's. A
/// pure-ASCII query is also looked for in raw bytes first ([`Finder::prefilter`]),
/// which is what rg did for the app.
pub struct Finder {
    needle: Vec<u16>,
    /// The query lowercased, when it is ASCII only.
    ascii: Option<Vec<u8>>,
}

impl Finder {
    pub fn new(query: &str) -> Finder {
        let ascii = query
            .is_ascii()
            .then(|| query.bytes().map(|b| b.to_ascii_lowercase()).collect());
        Finder {
            needle: query.encode_utf16().map(fold).collect(),
            ascii,
        }
    }

    /// The query's length in UTF-16 units: a match is as long.
    pub fn len(&self) -> usize {
        self.needle.len()
    }

    /// The first match in `text` at or after `from`.
    pub fn find(&self, text: &[u16], from: usize) -> Option<usize> {
        let n = self.needle.len();
        if n == 0 || text.len() < n || from > text.len() - n {
            return None;
        }
        (from..=text.len() - n).find(|&at| {
            text[at..at + n]
                .iter()
                .zip(&self.needle)
                .all(|(unit, want)| fold(*unit) == *want)
        })
    }

    /// Whether `bytes` (a transcript line, or a piece of text) has the
    /// query in it: the test rg made on every line.
    pub fn prefilter(&self, bytes: &[u8]) -> bool {
        match &self.ascii {
            Some(needle) => contains_ascii_ci(bytes, needle),
            None => {
                let text = String::from_utf8_lossy(bytes);
                self.find(&utf16(&text), 0).is_some()
            }
        }
    }
}

/// Whether `hay` has `needle` (already lowercase ASCII) in it, any case.
fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let lower = needle[0];
    let upper = lower.to_ascii_uppercase();
    let last = hay.len() - n;
    let mut at = 0;
    while at <= last {
        let Some(step) = hay[at..=last]
            .iter()
            .position(|&b| b == lower || b == upper)
        else {
            return false;
        };
        at += step;
        if hay[at..at + n]
            .iter()
            .zip(needle)
            .all(|(h, want)| h.to_ascii_lowercase() == *want)
        {
            return true;
        }
        at += 1;
    }
    false
}

/// Whether `hay` has the bytes `needle` in it.
pub fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let first = needle[0];
    let last = hay.len() - n;
    let mut at = 0;
    while at <= last {
        let Some(step) = hay[at..=last].iter().position(|&b| b == first) else {
            return false;
        };
        at += step;
        if &hay[at..at + n] == needle {
            return true;
        }
        at += 1;
    }
    false
}
