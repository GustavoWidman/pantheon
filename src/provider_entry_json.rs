//! Ephemeral ECMAScript JSON.parse -> JSON.stringify native values.
//! Never use for durable identities or journal metadata.
//! Objects retain first key position on duplicates; array-index keys enumerate
//! first. Strings retain UTF-16 code units, including lone surrogate escapes.
use anyhow::{Result, bail, ensure};

#[derive(Clone)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u16>),
    Array(Vec<Json>),
    Object(Vec<(Vec<u16>, Json)>),
}
impl std::fmt::Debug for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, size) = match self {
            Self::Null => ("null", 0),
            Self::Bool(_) => ("boolean", 1),
            Self::Number(_) => ("number", 1),
            Self::String(v) => ("string_utf16", v.len()),
            Self::Array(v) => ("array", v.len()),
            Self::Object(v) => ("object", v.len()),
        };
        f.debug_struct("NativeJsonShape")
            .field("kind", &kind)
            .field("units_or_members", &size)
            .finish()
    }
}
impl Json {
    pub fn parse(text: &str) -> Result<Self> {
        let mut p = Parser {
            bytes: text.as_bytes(),
            at: 0,
        };
        let value = p.value(0)?;
        p.space();
        ensure!(p.at == p.bytes.len(), "trailing observation JSON");
        Ok(value)
    }
    /// JSON.parse receives a JavaScript string, not necessarily scalar UTF8.
    /// Quote raw unpaired code units only *inside* a JSON string; a code unit
    /// outside strings or immediately after an escape remains invalid syntax.
    pub(crate) fn parse_utf16(units: &[u16]) -> Result<Self> {
        use std::fmt::Write;
        let mut text = String::new();
        let (mut quoted, mut escaped) = (false, false);
        for scalar in char::decode_utf16(units.iter().copied()) {
            match scalar {
                Err(e) => {
                    ensure!(
                        quoted && !escaped,
                        "invalid unpaired code unit in JSON syntax"
                    );
                    write!(&mut text, "\\u{:04x}", e.unpaired_surrogate()).unwrap();
                }
                Ok(c) => {
                    text.push(c);
                    if escaped {
                        escaped = false;
                    } else if c == '\\' && quoted {
                        escaped = true;
                    } else if c == '"' {
                        quoted = !quoted;
                    }
                }
            }
        }
        Self::parse(&text)
    }
    pub(crate) fn scalar_compatible(&self) -> bool {
        let string = |units: &[u16]| char::decode_utf16(units.iter().copied()).all(|c| c.is_ok());
        match self {
            Self::Number(v) => v.is_finite(),
            Self::String(v) => string(v),
            Self::Array(v) => v.iter().all(Self::scalar_compatible),
            Self::Object(v) => v.iter().all(|(k, v)| string(k) && v.scalar_compatible()),
            _ => true,
        }
    }
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(v) => *v,
            Self::Number(v) => *v != 0.0 && !v.is_nan(),
            Self::String(v) => !v.is_empty(),
            _ => true,
        }
    }
    pub fn get(&self, key: &str) -> Option<&Self> {
        let Self::Object(fields) = self else {
            return None;
        };
        fields
            .iter()
            .find(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
            .map(|(_, v)| v)
    }
    pub(crate) fn set(&mut self, key: &str, value: Self) {
        let Self::Object(fields) = self else {
            panic!("native set on nonobject")
        };
        let key: Vec<u16> = key.encode_utf16().collect();
        if let Some((_, old)) = fields.iter_mut().find(|(k, _)| k == &key) {
            *old = value;
        } else {
            fields.push((key, value));
        }
    }
    pub(crate) fn remove(&mut self, key: &str) {
        if let Self::Object(fields) = self {
            fields.retain(|(k, _)| !k.iter().copied().eq(key.encode_utf16()));
        }
    }
    pub fn stringify(&self) -> String {
        let mut text = String::new();
        self.write(&mut text);
        text
    }
    /// Fallible scalar-string projection for consumed Rust host fields only.
    /// Never authoritative for native execution input or continuation wire.
    pub fn typed(&self) -> Option<serde_json::Value> {
        use serde_json::{Map, Number, Value};
        Some(match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Number(value) => Value::Number(Number::from_f64(*value)?),
            Self::String(units) => Value::String(String::from_utf16(units).ok()?),
            Self::Array(values) => {
                Value::Array(values.iter().map(Self::typed).collect::<Option<_>>()?)
            }
            Self::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(k, v)| Some((String::from_utf16(k).ok()?, v.typed()?)))
                    .collect::<Option<Map<_, _>>>()?,
            ),
        })
    }
    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => {
                if value.is_finite() {
                    out.push_str(ryu_js::Buffer::new().format_finite(*value));
                } else {
                    out.push_str("null");
                }
            }
            Self::String(value) => string(value, out),
            Self::Array(values) => {
                out.push('[');
                for (i, value) in values.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    value.write(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                let mut indices = fields
                    .iter()
                    .filter_map(|(k, v)| array_index(k).map(|i| (i, k, v)))
                    .collect::<Vec<_>>();
                indices.sort_by_key(|(i, _, _)| *i);
                out.push('{');
                let mut comma = false;
                for (key, value) in indices.into_iter().map(|(_, k, v)| (k, v)).chain(
                    fields
                        .iter()
                        .filter(|(k, _)| array_index(k).is_none())
                        .map(|(k, v)| (k, v)),
                ) {
                    if comma {
                        out.push(',');
                    }
                    comma = true;
                    string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}
fn array_index(key: &[u16]) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key[0] == b'0' as u16) {
        return None;
    }
    let mut n = 0u32;
    for &c in key {
        if !(b'0' as u16..=b'9' as u16).contains(&c) {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((c - b'0' as u16).into())?;
    }
    (n != u32::MAX).then_some(n)
}
fn string(units: &[u16], out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    for c in char::decode_utf16(units.iter().copied()) {
        match c {
            Err(e) => {
                write!(out, "\\u{:04x}", e.unpaired_surrogate()).unwrap();
            }
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{08}') => out.push_str("\\b"),
            Ok('\u{0c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if c < '\u{20}' => {
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            Ok(c) => out.push(c),
        }
    }
    out.push('"');
}
struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| anyhow::anyhow!("short observation JSON"))?;
        self.at += 1;
        Ok(b)
    }
    fn value(&mut self, depth: usize) -> Result<Json> {
        ensure!(depth < 128, "observation JSON nesting safety boundary");
        self.space();
        let first = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| anyhow::anyhow!("missing observation JSON"))?;
        match first {
            b'n' | b't' | b'f' => {
                let (word, value) = match first {
                    b'n' => (b"null".as_slice(), Json::Null),
                    b't' => (b"true".as_slice(), Json::Bool(true)),
                    _ => (b"false".as_slice(), Json::Bool(false)),
                };
                ensure!(
                    self.bytes.get(self.at..self.at + word.len()) == Some(word),
                    "invalid observation literal"
                );
                self.at += word.len();
                Ok(value)
            }
            b'"' => Ok(Json::String(self.string()?)),
            b'[' | b'{' => {
                self.at += 1;
                let object = first == b'{';
                let end = if object { b'}' } else { b']' };
                let mut fields: Vec<(Vec<u16>, Json)> = Vec::new();
                let mut values = Vec::new();
                self.space();
                if self.bytes.get(self.at) == Some(&end) {
                    self.at += 1;
                } else {
                    loop {
                        if object {
                            self.space();
                            let key = self.string()?;
                            self.space();
                            ensure!(self.byte()? == b':', "missing observation colon");
                            let value = self.value(depth + 1)?;
                            if let Some((_, old)) = fields.iter_mut().find(|(k, _)| *k == key) {
                                *old = value;
                            } else {
                                fields.push((key, value));
                            }
                        } else {
                            values.push(self.value(depth + 1)?);
                        }
                        self.space();
                        match self.byte()? {
                            b if b == end => break,
                            b',' => {}
                            _ => bail!("invalid observation separator"),
                        }
                    }
                }
                Ok(if object {
                    Json::Object(fields)
                } else {
                    Json::Array(values)
                })
            }
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                if first == b'-' {
                    self.at += 1;
                }
                match self.byte()? {
                    b'0' => {}
                    b'1'..=b'9' => {
                        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                            self.at += 1;
                        }
                    }
                    _ => bail!("invalid observation number"),
                }
                if self.bytes.get(self.at) == Some(&b'.') {
                    self.at += 1;
                    self.digits()?;
                }
                if self
                    .bytes
                    .get(self.at)
                    .is_some_and(|b| matches!(b, b'e' | b'E'))
                {
                    self.at += 1;
                    if self
                        .bytes
                        .get(self.at)
                        .is_some_and(|b| matches!(b, b'+' | b'-'))
                    {
                        self.at += 1;
                    }
                    self.digits()?;
                }
                Ok(Json::Number(
                    std::str::from_utf8(&self.bytes[start..self.at])?.parse()?,
                ))
            }
            _ => bail!("invalid observation JSON"),
        }
    }
    fn digits(&mut self) -> Result<()> {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        ensure!(self.at != start, "missing observation number digits");
        Ok(())
    }
    fn string(&mut self) -> Result<Vec<u16>> {
        ensure!(self.byte()? == b'"', "missing observation string");
        let mut units = Vec::new();
        loop {
            match self.byte()? {
                b'"' => return Ok(units),
                b'\\' => match self.byte()? {
                    b'"' => units.push(b'"'.into()),
                    b'\\' => units.push(b'\\'.into()),
                    b'/' => units.push(b'/'.into()),
                    b'b' => units.push(8),
                    b'f' => units.push(12),
                    b'n' => units.push(10),
                    b'r' => units.push(13),
                    b't' => units.push(9),
                    b'u' => {
                        let mut n = 0;
                        for _ in 0..4 {
                            n = n * 16
                                + (self.byte()? as char)
                                    .to_digit(16)
                                    .ok_or_else(|| anyhow::anyhow!("invalid observation escape"))?
                                    as u16;
                        }
                        units.push(n);
                    }
                    _ => bail!("invalid observation escape"),
                },
                b if b < 0x20 => bail!("invalid observation control"),
                b if b < 0x80 => units.push(b.into()),
                _ => {
                    self.at -= 1;
                    let c = std::str::from_utf8(&self.bytes[self.at..])?
                        .chars()
                        .next()
                        .unwrap();
                    self.at += c.len_utf8();
                    units.extend(c.encode_utf16(&mut [0; 2]).iter().copied());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ecmascript_observation_recursive_order_duplicates_numbers_and_surrogates() {
        for (input, output) in [
            (
                r#"{"z":1,"a":2,"z":3,"2":2,"1":1,"01":4,"4294967295":5,"4294967294":6}"#,
                r#"{"1":1,"2":2,"4294967294":6,"z":3,"a":2,"01":4,"4294967295":5}"#,
            ),
            (
                r#"{"n":[-0,1e-7,1e-6,1e20,1e21,9007199254740993,1e400],"s":"\ud800\udc00\ud800\u0000/\n"}"#,
                "{\"n\":[0,1e-7,0.000001,100000000000000000000,1e+21,9007199254740992,null],\"s\":\"𐀀\\ud800\\u0000/\\n\"}",
            ),
        ] {
            assert_eq!(Json::parse(input).unwrap().stringify(), output);
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(r#"{"z":1,"a":2}"#)
                .unwrap()
                .to_string(),
            r#"{"a":2,"z":1}"#,
            "global durable/cache serde order must stay unchanged"
        );
        assert!(Json::parse(r#"{"s":"\ud800"}"#).unwrap().typed().is_none());
        assert!(Json::parse(r#"{"n":1e400}"#).unwrap().typed().is_none());
        for bad in [
            "",
            "[1,]",
            "{\"a\":}",
            "01",
            "1.",
            "1e",
            "true x",
            "\"\\x00\"",
        ] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
    }
}
