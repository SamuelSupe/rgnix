use serde::Deserializer;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;

// Borrow raw subtrees and walk one member at a time: routing must not allocate a
// JSON DOM whose size can be many times larger than the bounded request body.
pub fn value_at<'a>(bytes: &'a [u8], pointer: &str) -> Option<&'a RawValue> {
    if pointer.len() > 1024 || (!pointer.is_empty() && !pointer.starts_with('/')) {
        return None;
    }
    let mut value = serde_json::from_slice::<&RawValue>(bytes).ok()?;
    if !pointer.is_empty() {
        let mut count = 0;
        for component in pointer[1..].split('/') {
            count += 1;
            if count > 32 {
                return None;
            }
            let mut key = String::new();
            let mut chars = component.chars();
            while let Some(c) = chars.next() {
                key.push(if c == '~' {
                    match chars.next()? {
                        '0' => '~',
                        '1' => '/',
                        _ => return None,
                    }
                } else {
                    c
                });
            }
            let mut deserializer = serde_json::Deserializer::from_str(value.get());
            value = match value.get().as_bytes().first() {
                Some(b'{') => deserializer.deserialize_map(Member(&key)).ok()??,
                Some(b'[') if key == "0" || (!key.starts_with('0') && !key.is_empty()) => {
                    if !key.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    deserializer
                        .deserialize_seq(Element(key.parse().ok()?))
                        .ok()??
                }
                _ => return None,
            };
        }
    }
    Some(value)
}

struct Member<'a>(&'a str);
impl<'de> Visitor<'de> for Member<'_> {
    type Value = Option<&'de RawValue>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value::<&RawValue>()?;
            if key == self.0 {
                selected = Some(value);
            }
        }
        Ok(selected)
    }
}

struct Element(usize);
impl<'de> Visitor<'de> for Element {
    type Value = Option<&'de RawValue>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array")
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        let mut index = 0;
        let mut selected = None;
        while let Some(value) = seq.next_element::<&RawValue>()? {
            if index == self.0 {
                selected = Some(value);
            }
            index += 1;
        }
        Ok(selected)
    }
}
