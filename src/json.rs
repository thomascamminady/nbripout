//! A small JSON reader/writer that reproduces CPython's `json` module byte for byte.
//!
//! nbformat (and therefore nbstripout) reads notebooks with `json.loads` and writes
//! them with `json.dumps(..., indent=1, sort_keys=True, separators=(",", ": "))`.
//! A drop-in replacement has to emit identical bytes, which off-the-shelf JSON crates
//! cannot do: Python accepts `NaN`/`Infinity`, renders floats with `repr()` (`1e-05`,
//! `1e+16`, `100.0`), keeps integers at arbitrary precision and has its own escaping
//! rules for `ensure_ascii`.

use std::fmt;

use indexmap::IndexMap;

/// JSON object. Insertion ordered, duplicate keys keep their first position and
/// their last value, exactly like a Python `dict` built by `json.loads`.
pub type Map = IndexMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

/// A JSON number with Python semantics: literals without fraction or exponent are
/// arbitrary-precision integers, everything else is an IEEE-754 double.
#[derive(Debug, Clone, PartialEq)]
pub enum Number {
    /// Canonical decimal text of the integer (`-0` is normalized to `0`).
    Int(Box<str>),
    Float(f64),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Maximum nesting depth accepted by the parser. CPython's recursion limit makes
/// `json.loads` fail at a similar depth, and the bound keeps every recursive pass
/// over the tree safely within the stack.
pub const MAX_DEPTH: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: &'static str,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at line {} column {}",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse a JSON document with the grammar accepted by Python's `json.loads`.
///
/// Lone UTF-16 surrogates (`"\ud800"`) are rejected: Python accepts them but cannot
/// write them back as UTF-8, which is exactly where nbstripout ends up truncating
/// the notebook it was rewriting.
pub fn parse(src: &str) -> Result<Value, ParseError> {
    let mut p = Parser {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.error("extra data after JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> ParseError {
        let before = &self.bytes[..self.pos.min(self.bytes.len())];
        let line_start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        ParseError {
            message,
            line: before.iter().filter(|&&b| b == b'\n').count() + 1,
            // Count characters, not bytes: skip UTF-8 continuation bytes.
            column: before[line_start..]
                .iter()
                .filter(|&&b| (b & 0xC0) != 0x80)
                .count()
                + 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'N') => self.literal("NaN", Value::Number(Number::Float(f64::NAN))),
            Some(b'I') => self.literal("Infinity", Value::Number(Number::Float(f64::INFINITY))),
            Some(b'-') if self.bytes.get(self.pos + 1) == Some(&b'I') => {
                self.literal("-Infinity", Value::Number(Number::Float(f64::NEG_INFINITY)))
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expecting value")),
        }
    }

    fn literal(&mut self, word: &'static str, value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("expecting value"))
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("maximum nesting depth exceeded"));
        }
        Ok(())
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                self.skip_ws();
                items.push(self.value()?);
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.error("expecting ',' or ']'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut map = Map::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                self.skip_ws();
                if self.peek() != Some(b'"') {
                    return Err(self.error("expecting property name enclosed in double quotes"));
                }
                let key = self.string()?;
                self.skip_ws();
                if self.peek() != Some(b':') {
                    return Err(self.error("expecting ':' delimiter"));
                }
                self.pos += 1;
                self.skip_ws();
                let value = self.value()?;
                map.insert(key, value);
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.error("expecting ',' or '}'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Object(map))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        let mut chunk_start = self.pos;
        loop {
            let rest = &self.bytes[self.pos..];
            let Some(offset) = memchr::memchr2(b'"', b'\\', rest) else {
                self.pos = self.bytes.len();
                return Err(self.error("unterminated string"));
            };
            // Python's strict mode rejects raw control characters inside strings.
            // `fold` instead of `any` so the common (clean) case vectorizes.
            let chunk = &rest[..offset];
            if chunk.iter().fold(false, |acc, &b| acc | (b < 0x20)) {
                self.pos += chunk.iter().position(|&b| b < 0x20).unwrap_or(0);
                return Err(self.error("invalid control character in string"));
            }
            self.pos += offset;
            out.push_str(&self.src[chunk_start..self.pos]);
            if self.bytes[self.pos] == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            self.escape(&mut out)?;
            chunk_start = self.pos;
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let start = self.pos;
        let Some(c) = self.bytes.get(self.pos + 1).copied() else {
            self.pos = self.bytes.len();
            return Err(self.error("unterminated string"));
        };
        self.pos += 2;
        let decoded = match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let unit = self.hex4()?;
                let code = match unit {
                    0xD800..=0xDBFF if self.bytes[self.pos..].starts_with(b"\\u") => {
                        self.pos += 2;
                        let low = self.hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            self.pos = start;
                            return Err(self.error("lone UTF-16 surrogate in string"));
                        }
                        0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                    }
                    0xD800..=0xDFFF => {
                        self.pos = start;
                        return Err(self.error("lone UTF-16 surrogate in string"));
                    }
                    _ => unit,
                };
                char::from_u32(code).expect("surrogates are handled above")
            }
            _ => {
                self.pos = start;
                return Err(self.error("invalid \\escape"));
            }
        };
        out.push(decoded);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .filter(|d| d.iter().all(u8::is_ascii_hexdigit));
        let Some(digits) = digits else {
            return Err(self.error("invalid \\uXXXX escape"));
        };
        let text = std::str::from_utf8(digits).expect("hex digits are ASCII");
        self.pos += 4;
        Ok(u32::from_str_radix(text, 16).expect("validated hex digits"))
    }

    /// Python's number grammar: `-?(0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`.
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.skip_digits(),
            _ => return Err(self.error("expecting value")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            is_float = true;
            self.pos += 1;
            self.skip_digits();
        }
        if let Some(b'e' | b'E') = self.peek() {
            let mut end = self.pos + 1;
            if let Some(b'+' | b'-') = self.bytes.get(end) {
                end += 1;
            }
            if self.bytes.get(end).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                self.pos = end;
                self.skip_digits();
            }
        }
        let text = &self.src[start..self.pos];
        let number = if is_float {
            Number::Float(text.parse().expect("validated float literal"))
        } else if text == "-0" {
            Number::Int("0".into())
        } else {
            Number::Int(text.into())
        };
        Ok(Value::Number(number))
    }

    fn skip_digits(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
    }
}

/// Output conventions of `json.dumps`, parameterized the way nbformat and
/// nbstripout's Zeppelin mode call it.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub indent: usize,
    pub sort_keys: bool,
    pub ensure_ascii: bool,
}

impl Style {
    /// nbformat v4 writer: `indent=1, sort_keys=True, ensure_ascii=False`.
    pub const NBFORMAT_V4: Style = Style {
        indent: 1,
        sort_keys: true,
        ensure_ascii: false,
    };
    /// nbformat v2/v3 writer: like v4 but keeps `json.dumps`' default `ensure_ascii=True`.
    pub const NBFORMAT_V3: Style = Style {
        indent: 1,
        sort_keys: true,
        ensure_ascii: true,
    };
    /// nbstripout's Zeppelin writer: `json.dump(nb, indent=2)`.
    pub const ZEPPELIN: Style = Style {
        indent: 2,
        sort_keys: false,
        ensure_ascii: true,
    };
}

/// Serialize `value` like `json.dumps(value, indent=..., separators=(",", ": "))`.
pub fn write(out: &mut Vec<u8>, value: &Value, style: Style) {
    write_value(out, value, style, 0);
}

fn write_value(out: &mut Vec<u8>, value: &Value, style: Style, level: usize) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(Number::Int(digits)) => out.extend_from_slice(digits.as_bytes()),
        Value::Number(Number::Float(x)) => out.extend_from_slice(json_float(*x).as_bytes()),
        Value::String(s) => write_str(out, s, style.ensure_ascii),
        Value::Array(items) if items.is_empty() => out.extend_from_slice(b"[]"),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                newline(out, style, level + 1);
                write_value(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push(b']');
        }
        Value::Object(map) if map.is_empty() => out.extend_from_slice(b"{}"),
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if style.sort_keys {
                // Byte order of UTF-8 equals code point order, i.e. Python's `sorted`.
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            }
            out.push(b'{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                newline(out, style, level + 1);
                write_str(out, key, style.ensure_ascii);
                out.extend_from_slice(b": ");
                write_value(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push(b'}');
        }
    }
}

fn newline(out: &mut Vec<u8>, style: Style, level: usize) {
    out.push(b'\n');
    out.resize(out.len() + style.indent * level, b' ');
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn write_u_escape(out: &mut Vec<u8>, unit: u32) {
    out.extend_from_slice(b"\\u");
    for shift in [12, 8, 4, 0] {
        out.push(HEX[((unit >> shift) & 0xF) as usize]);
    }
}

fn short_escape(b: u8) -> Option<&'static [u8]> {
    Some(match b {
        b'"' => b"\\\"",
        b'\\' => b"\\\\",
        b'\n' => b"\\n",
        b'\r' => b"\\r",
        b'\t' => b"\\t",
        0x08 => b"\\b",
        0x0C => b"\\f",
        _ => return None,
    })
}

fn write_str(out: &mut Vec<u8>, s: &str, ensure_ascii: bool) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    if ensure_ascii {
        // Everything outside printable ASCII (including DEL) becomes \uXXXX,
        // astral characters as a surrogate pair.
        for (i, ch) in s.char_indices() {
            if (' '..='~').contains(&ch) && ch != '"' && ch != '\\' {
                continue;
            }
            out.extend_from_slice(&bytes[start..i]);
            start = i + ch.len_utf8();
            let esc = if ch.is_ascii() {
                short_escape(ch as u8)
            } else {
                None
            };
            if let Some(esc) = esc {
                out.extend_from_slice(esc);
            } else {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    write_u_escape(out, u32::from(*unit));
                }
            }
        }
    } else {
        for (i, &b) in bytes.iter().enumerate() {
            if b >= 0x20 && b != b'"' && b != b'\\' {
                continue;
            }
            out.extend_from_slice(&bytes[start..i]);
            start = i + 1;
            match short_escape(b) {
                Some(esc) => out.extend_from_slice(esc),
                None => write_u_escape(out, u32::from(b)),
            }
        }
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// `json.dumps` rendering of a float (`allow_nan=True`).
pub fn json_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_owned()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        py_float_repr(x)
    }
}

/// Split Rust's scientific formatting into significant digits and exponent.
fn sci_parts(sci: &str) -> (Vec<u8>, i32) {
    let (mantissa, exponent) = sci.split_once('e').expect("scientific notation");
    let digits = mantissa.bytes().filter(|&b| b != b'.').collect();
    (digits, exponent.parse().expect("integer exponent"))
}

/// The shortest digits that round-trip to `x` (positive, finite) and the decimal
/// exponent of the first digit, chosen exactly like CPython's dtoa.
///
/// Rust's `{:e}` finds the same shortest digits, except when `x` lies exactly
/// halfway between two equally short candidates: Rust rounds up, CPython rounds
/// half to even (`726104781533262.25` is `...262.2` in Python). Such ties need 16
/// or more significant digits, so the exact expansion is only checked then.
fn shortest_digits(x: f64) -> (String, i32) {
    let (digits, exponent) = sci_parts(&format!("{x:e}"));
    let n = digits.len();
    let as_string = |d: Vec<u8>| String::from_utf8(d).expect("ASCII digits");
    if n < 16 {
        return (as_string(digits), exponent);
    }
    // 767 significant digits suffice for the exact expansion of any f64.
    let (exact, exact_exponent) = sci_parts(&format!("{x:.800e}"));
    let is_tie =
        exact_exponent == exponent && exact[n] == b'5' && exact[n + 1..].iter().all(|&b| b == b'0');
    if !is_tie {
        return (as_string(digits), exponent);
    }
    let below = exact[..n].to_vec();
    let mut above = below.clone();
    for digit in above.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            break;
        }
    }
    let round_trips = |d: &[u8]| {
        let text = format!("{}e{}", as_string(d.to_vec()), exponent - (n as i32 - 1));
        text.parse::<f64>() == Ok(x)
    };
    let mut candidates = [below, above];
    // Even last digit first; at a binade boundary only one side may round-trip.
    candidates.sort_by_key(|d| d[n - 1] % 2);
    match candidates
        .into_iter()
        .find(|d| d[0] != b'0' && round_trips(d))
    {
        Some(chosen) => {
            let trimmed = chosen.len() - chosen.iter().rev().take_while(|&&b| b == b'0').count();
            (as_string(chosen[..trimmed.max(1)].to_vec()), exponent)
        }
        None => (as_string(digits), exponent),
    }
}

/// Python's `repr(float)` for finite values: the shortest round-tripping digits,
/// in positional notation for 1e-4 <= |x| < 1e16 and scientific notation otherwise.
pub fn py_float_repr(x: f64) -> String {
    debug_assert!(x.is_finite());
    let (digits, exponent) = shortest_digits(x.abs());
    // Position of the decimal point relative to the start of `digits`.
    let decpt = exponent + 1;
    let n = digits.len() as i32;

    let mut s = String::with_capacity(digits.len() + 8);
    if x.is_sign_negative() {
        s.push('-');
    }
    if !(-4 < decpt && decpt <= 16) {
        s.push_str(&digits[..1]);
        if n > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        s.push_str(&format!("e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs()));
    } else if decpt <= 0 {
        s.push_str("0.");
        s.extend(std::iter::repeat('0').take(decpt.unsigned_abs() as usize));
        s.push_str(&digits);
    } else if decpt >= n {
        s.push_str(&digits);
        s.extend(std::iter::repeat('0').take((decpt - n) as usize));
        s.push_str(".0");
    } else {
        s.push_str(&digits[..decpt as usize]);
        s.push('.');
        s.push_str(&digits[decpt as usize..]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &str, style: Style) -> String {
        let mut out = Vec::new();
        write(&mut out, &parse(src).unwrap(), style);
        String::from_utf8(out).unwrap()
    }

    #[test]
    #[allow(clippy::excessive_precision)] // the exact binary values are the point
    fn float_repr_matches_python() {
        let cases = [
            (1e-5, "1e-05"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (1.5e300, "1.5e+300"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (2500.0, "2500.0"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e22, "1e+22"),
            (5e-324, "5e-324"),
            (123.456, "123.456"),
            (-1.25e-7, "-1.25e-07"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            // Exact ties between two shortest candidates round half to even.
            (726104781533262.25, "726104781533262.2"),
            (726104781533262.75, "726104781533262.8"),
        ];
        for (x, expected) in cases {
            assert_eq!(py_float_repr(x), expected, "repr({x:e})");
        }
        assert_eq!(json_float(f64::NAN), "NaN");
        assert_eq!(json_float(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn numbers_follow_python_semantics() {
        let out = roundtrip(
            "[1e400, -0, -0.0, 2.5E+3, 123456789012345678901234567890, NaN, -Infinity]",
            Style::NBFORMAT_V4,
        );
        assert_eq!(
            out,
            "[\n Infinity,\n 0,\n -0.0,\n 2500.0,\n 123456789012345678901234567890,\n NaN,\n -Infinity\n]"
        );
    }

    #[test]
    fn pretty_printing_matches_json_dumps() {
        let out = roundtrip(
            r#"{"b": [], "a": {"y": {}, "x": [1, "s"]}}"#,
            Style::NBFORMAT_V4,
        );
        assert_eq!(
            out,
            "{\n \"a\": {\n  \"x\": [\n   1,\n   \"s\"\n  ],\n  \"y\": {}\n },\n \"b\": []\n}"
        );
        let out = roundtrip(r#"{"b": 1, "a": 2}"#, Style::ZEPPELIN);
        assert_eq!(out, "{\n  \"b\": 1,\n  \"a\": 2\n}");
    }

    #[test]
    fn string_escaping() {
        let src = r#""\u007f\u0001\u001f\u2028é😀\b\f\n\r\t\"\\\/""#;
        assert_eq!(
            roundtrip(src, Style::NBFORMAT_V4),
            "\"\u{7f}\\u0001\\u001f\u{2028}é😀\\b\\f\\n\\r\\t\\\"\\\\/\""
        );
        assert_eq!(
            roundtrip(src, Style::NBFORMAT_V3),
            r#""\u007f\u0001\u001f\u2028\u00e9\ud83d\ude00\b\f\n\r\t\"\\/""#
        );
    }

    #[test]
    fn duplicate_keys_keep_first_position_last_value() {
        let out = roundtrip(r#"{"b": 1, "a": 2, "b": 3}"#, Style::ZEPPELIN);
        assert_eq!(out, "{\n  \"b\": 3,\n  \"a\": 2\n}");
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "",
            "[1,]",
            "{\"a\" 1}",
            "[01]",
            "[1.]",
            "\"\\ud800\"",
            "\"\\udc00x\"",
            "\"a\u{1}\"",
            "\"\\x\"",
            "[1] 2",
            "{'a': 1}",
            "nul",
            "\"abc",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        let err = parse("{\n  \"a\": [1,\n  ]\n}").unwrap_err();
        assert_eq!((err.line, err.column), (3, 3));
    }

    #[test]
    fn nesting_depth_is_bounded() {
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(&ok).is_ok());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(parse(&deep).is_err());
    }
}
