//! Python built-in semantics that nbstripout's behavior depends on.

use crate::json::{py_float_repr, Number, Value};

/// `str.isspace()` for a single character. Rust's `char::is_whitespace` follows the
/// Unicode `White_Space` property, which lacks the ASCII separators U+001C..U+001F
/// that Python counts as whitespace.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `s.split()`: split on runs of whitespace, dropping empty pieces.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|piece| !piece.is_empty())
}

/// `s.splitlines(keepends)`, including Python's full set of line boundaries
/// (`\v`, `\f`, `\x1c`-`\x1e`, `\x85`, U+2028, U+2029 besides `\n`, `\r`, `\r\n`).
pub fn splitlines(s: &str, keepends: bool) -> Vec<Value> {
    let bytes = s.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let width = match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => 2,
            b'\n' | b'\r' | 0x0B | 0x0C | 0x1C..=0x1E => 1,
            // U+0085 is C2 85; U+2028/U+2029 are E2 80 A8/A9. Lead bytes cannot be
            // confused with continuation bytes, so byte matching is exact.
            0xC2 if bytes.get(i + 1) == Some(&0x85) => 2,
            0xE2 if bytes.get(i + 1) == Some(&0x80)
                && matches!(bytes.get(i + 2), Some(0xA8 | 0xA9)) =>
            {
                3
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let end = if keepends { i + width } else { i };
        lines.push(Value::String(s[start..end].to_owned()));
        i += width;
        start = i;
    }
    if start < bytes.len() {
        lines.push(Value::String(s[start..].to_owned()));
    }
    lines
}

/// `bool(value)`.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(Number::Int(digits)) => &**digits != "0",
        // NaN is truthy in Python, and `NaN != 0.0` holds.
        Value::Number(Number::Float(x)) => *x != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `needle in container` for a string needle. Python raises `TypeError` for
/// containers that do not support `in`; those count as "not contained" here.
pub fn contains(container: &Value, needle: &str) -> bool {
    match container {
        Value::Array(items) => items.iter().any(|item| item.as_str() == Some(needle)),
        Value::String(s) => s.contains(needle),
        Value::Object(map) => map.contains_key(needle),
        _ => false,
    }
}

/// `len(str(s.encode("ascii")))`, i.e. the length of a bytes literal such as
/// `b'iVBOR\\n'`. nbformat v2 holds PNG/JPEG data as bytes, which matters for
/// nbstripout's size accounting.
pub fn bytes_repr_len(s: &str) -> usize {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    3 + s
        .chars()
        .map(|c| match c {
            '\\' | '\t' | '\n' | '\r' => 2,
            c if c == quote => 2,
            c if (c as u32) < 0x20 || c == '\x7f' => 4,
            _ => 1,
        })
        .sum::<usize>()
}

/// `len(str(value))` for scalars, as used by nbstripout's `get_size`.
fn str_len(value: &Value) -> usize {
    match value {
        Value::Null => "None".len(),
        Value::Bool(true) => "True".len(),
        Value::Bool(false) => "False".len(),
        Value::Number(Number::Int(digits)) => digits.len(),
        Value::Number(Number::Float(x)) if x.is_nan() => "nan".len(),
        Value::Number(Number::Float(x)) if x.is_infinite() => {
            if *x > 0.0 {
                "inf".len()
            } else {
                "-inf".len()
            }
        }
        Value::Number(Number::Float(x)) => py_float_repr(*x).len(),
        Value::String(s) => s.chars().count(),
        Value::Array(_) | Value::Object(_) => unreachable!("containers are summed"),
    }
}

/// Whether nbstripout's `get_size(value) <= limit`, where `get_size` sums the
/// lengths of all strings (in characters) and of `str()` of all other scalars.
/// Stops counting as soon as the limit is exceeded, so huge base64 payloads are
/// not scanned when the default limit of 0 applies.
pub fn size_at_most(value: &Value, limit: i64) -> bool {
    fn consume(value: &Value, budget: &mut i64) -> bool {
        match value {
            Value::Array(items) => items.iter().all(|item| consume(item, budget)),
            Value::Object(map) => map.values().all(|item| consume(item, budget)),
            scalar => {
                *budget -= i64::try_from(str_len(scalar)).unwrap_or(i64::MAX);
                *budget >= 0
            }
        }
    }
    let mut budget = limit;
    budget >= 0 && consume(value, &mut budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn strings(values: Vec<Value>) -> Vec<String> {
        values
            .into_iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn splitlines_matches_python() {
        let s = "a\x1cb\x1dc\u{85}d\u{2028}e\u{2029}f\x0bg\x0ch\ri\r\nj";
        assert_eq!(
            strings(splitlines(s, true)),
            [
                "a\x1c",
                "b\x1d",
                "c\u{85}",
                "d\u{2028}",
                "e\u{2029}",
                "f\x0b",
                "g\x0c",
                "h\r",
                "i\r\n",
                "j"
            ]
        );
        assert!(splitlines("", true).is_empty());
        assert_eq!(strings(splitlines("x\n", true)), ["x\n"]);
        assert_eq!(strings(splitlines("\n\n", true)), ["\n", "\n"]);
        assert_eq!(strings(splitlines("é\u{2027}ü", true)), ["é\u{2027}ü"]);
        assert_eq!(strings(splitlines("a\r\nb\n", false)), ["a", "b"]);
        assert_eq!(strings(splitlines("\n\nc", false)), ["", "", "c"]);
    }

    #[test]
    fn whitespace_matches_python() {
        assert!(is_space('\u{1f}') && is_space('\u{3000}') && is_space('\u{85}'));
        assert!(!is_space('\u{200b}') && !is_space('x'));
        assert_eq!(
            split_whitespace(" a\u{1c}b  c ").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn get_size_semantics() {
        let v =
            parse(r#"{"output_type": "stream", "n": null, "t": true, "x": [1.5, -20]}"#).unwrap();
        // "stream" (6) + "None" (4) + "True" (4) + "1.5" (3) + "-20" (3) = 20
        assert!(size_at_most(&v, 20));
        assert!(!size_at_most(&v, 19));
        assert!(size_at_most(&parse("{}").unwrap(), 0));
        assert!(!size_at_most(&parse("{}").unwrap(), -1));
        assert!(size_at_most(&parse(r#""ééé""#).unwrap(), 3));
        assert_eq!(bytes_repr_len("ab\ncd"), 9); // b'ab\ncd'
        assert_eq!(bytes_repr_len("it's"), 7); // b"it's"
        assert_eq!(bytes_repr_len("\"'"), 6); // b'"\''
    }
}
