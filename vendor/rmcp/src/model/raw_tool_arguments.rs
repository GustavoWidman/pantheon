//! Header-only scalar projection of the same owned raw arguments sent on wire.
//! Nested values remain opaque RawValue tokens, so unrelated escaped UTF16
//! cannot make a valid scalar annotation disappear. No JS/body reserialization.
use super::JsonObject;
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;

pub(super) fn projection(text: &str) -> Result<(JsonObject, Vec<String>), serde_json::Error> {
    fn space(text: &str, at: &mut usize) {
        while text
            .as_bytes()
            .get(*at)
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            *at += 1;
        }
    }
    fn token(text: &str, at: &mut usize) -> Result<Box<RawValue>, serde_json::Error> {
        let mut stream =
            serde_json::Deserializer::from_str(&text[*at..]).into_iter::<Box<RawValue>>();
        let raw = stream.next().expect("validated raw JSON token")?;
        *at += stream.byte_offset();
        Ok(raw)
    }
    // The constructor already validates the complete JSON. The MCP envelope
    // permits absent/null args; only object fields have scalar annotations.
    let mut at = 0;
    space(text, &mut at);
    if text.as_bytes().get(at) != Some(&b'{') {
        return Ok((JsonObject::new(), Vec::new()));
    }
    at += 1;
    let mut fields = BTreeMap::<String, (Option<Value>, bool)>::new();
    loop {
        space(text, &mut at);
        if text.as_bytes().get(at) == Some(&b'}') {
            break;
        }
        let key = token(text, &mut at)?;
        space(text, &mut at);
        debug_assert_eq!(text.as_bytes().get(at), Some(&b':'));
        at += 1;
        let raw = token(text, &mut at)?;
        if let Ok(key) = serde_json::from_str::<String>(key.get()) {
            let (value, unpaired) = match raw.get().as_bytes().first() {
                Some(b'"') => match serde_json::from_str::<String>(raw.get()) {
                    Ok(value) => (Some(Value::String(value)), false),
                    Err(_) => (None, true), // syntactically valid lone UTF16 scalar
                },
                Some(b't' | b'f' | b'-' | b'0'..=b'9') => {
                    (Some(serde_json::from_str(raw.get())?), false)
                }
                _ => (None, false), // null, arrays and objects do not get headers
            };
            // Last duplicate wins; an earlier unpaired scalar cannot poison a
            // later valid value. The body retains its original canonical bytes.
            fields.insert(key, (value, unpaired));
        }
        space(text, &mut at);
        if text.as_bytes().get(at) == Some(&b',') {
            at += 1;
        } else {
            break;
        }
    }
    let mut projected = JsonObject::new();
    let mut unpaired = Vec::new();
    for (key, (value, invalid)) in fields {
        if invalid {
            unpaired.push(key.clone());
        }
        if let Some(value) = value {
            projected.insert(key, value);
        }
    }
    Ok((projected, unpaired))
}
