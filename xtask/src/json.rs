//! A small JSON reader/writer.
//!
//! The release transaction has two external data sources it must read exactly:
//! the dogfood lab's `release.json` / `L4.json` / `rc.json` evidence, and `gh`
//! output. It also has to write its own state and report. `xtask` deliberately
//! has no dependencies, so this is a self-contained implementation rather than
//! a new crate. It is a reader/writer for *data we must not misread*, not a
//! general-purpose library: no borrowed views, no streaming, no ordering
//! guarantees beyond insertion order.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(text: impl Into<String>) -> Json {
        Json::Str(text.into())
    }

    pub fn num(value: f64) -> Json {
        Json::Num(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn index(&self, position: usize) -> Option<&Json> {
        match self {
            Json::Arr(items) => items.get(position),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(pairs) => Some(pairs),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// `get` where a missing key is `Json::Null` instead of `None`. Reading
    /// external evidence should not fall over on one absent field.
    pub fn at(&self, key: &str) -> &Json {
        self.get(key).unwrap_or(&Json::Null)
    }

    /// Dotted path lookup: `layers.L3.status`, `cases.0.id`.
    pub fn path(&self, path: &str) -> Option<&Json> {
        let mut node = self;
        for part in path.split('.') {
            node = match part.parse::<usize>() {
                Ok(position) => node.index(position)?,
                Err(_) => node.get(part)?,
            };
        }
        Some(node)
    }

    pub fn path_str(&self, path: &str) -> Option<&str> {
        self.path(path)?.as_str()
    }

    pub fn path_f64(&self, path: &str) -> Option<f64> {
        self.path(path)?.as_f64()
    }

    pub fn parse(text: &str) -> Result<Json, String> {
        let bytes = text.as_bytes();
        let mut cursor = Cursor { bytes, at: 0 };
        cursor.skip_ws();
        let value = cursor.value()?;
        cursor.skip_ws();
        if cursor.at != bytes.len() {
            return Err(format!("trailing data at byte {}", cursor.at));
        }
        Ok(value)
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        write_pretty(self, 0, &mut out);
        out.push('\n');
        out
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn skip_ws(&mut self) {
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.at))
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(())
        } else {
            Err(format!("expected {word} at byte {}", self.at))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(_) => self.number(),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.eat(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let value = self.value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.at)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.at)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| "unterminated string".to_string())?;
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self
                        .peek()
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            // Surrogate pairs: a lone surrogate is replaced
                            // rather than fabricated into a valid scalar.
                            if (0xd800..0xdc00).contains(&code) {
                                if self.peek() == Some(b'\\')
                                    && self.bytes.get(self.at + 1) == Some(&b'u')
                                {
                                    self.at += 2;
                                    let low = self.hex4()?;
                                    if (0xdc00..0xe000).contains(&low) {
                                        let combined =
                                            0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                                        out.push(
                                            char::from_u32(combined)
                                                .unwrap_or(char::REPLACEMENT_CHARACTER),
                                        );
                                        continue;
                                    }
                                }
                                out.push(char::REPLACEMENT_CHARACTER);
                            } else {
                                out.push(
                                    char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER),
                                );
                            }
                        }
                        other => {
                            return Err(format!("bad escape {:?}", other as char));
                        }
                    }
                }
                other => {
                    // Multi-byte UTF-8 passes through untouched: collect the
                    // continuation bytes and reinterpret.
                    if other < 0x80 {
                        out.push(other as char);
                    } else {
                        let width = match other {
                            0xc0..=0xdf => 2,
                            0xe0..=0xef => 3,
                            _ => 4,
                        };
                        let start = self.at - 1;
                        self.at = start + width;
                        let slice = self
                            .bytes
                            .get(start..self.at)
                            .ok_or_else(|| "truncated utf-8".to_string())?;
                        out.push_str(&String::from_utf8_lossy(slice));
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let slice = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| "truncated \\u escape".to_string())?;
        self.at += 4;
        let text = std::str::from_utf8(slice).map_err(|_| "bad \\u escape".to_string())?;
        u32::from_str_radix(text, 16).map_err(|_| format!("bad \\u escape {text}"))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.at += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| "bad number".to_string())?;
        if text.is_empty() {
            return Err(format!("expected a value at byte {start}"));
        }
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("bad number {text}"))
    }
}

fn escape(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn number_text(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.2e18 {
        format!("{}", value as i64)
    } else if value.is_finite() {
        let text = format!("{value}");
        text
    } else {
        "null".to_string()
    }
}

fn write_compact(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(number) => out.push_str(&number_text(*number)),
        Json::Str(text) => escape(text, out),
        Json::Arr(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            out.push('{');
            for (index, (key, item)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                escape(key, out);
                out.push(':');
                write_compact(item, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(value: &Json, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let inner = "  ".repeat(depth + 1);
    match value {
        Json::Arr(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                out.push_str(&inner);
                write_pretty(item, depth + 1, out);
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(']');
        }
        Json::Obj(pairs) if !pairs.is_empty() => {
            out.push_str("{\n");
            for (index, (key, item)) in pairs.iter().enumerate() {
                out.push_str(&inner);
                escape(key, out);
                out.push_str(": ");
                write_pretty(item, depth + 1, out);
                if index + 1 < pairs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push('}');
        }
        other => write_compact(other, out),
    }
}
