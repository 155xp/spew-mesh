//! A JSON value that keeps key order, for editing agents' config files without reshuffling them (8).
//!
//! `serde_json::Value` sorts object keys unless the crate-wide `preserve_order` feature is on, and turning that on
//! would change every other module's JSON. This small type is enough for config edits.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// A JSON value whose objects keep their keys in file order.
#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl PartialEq for Json {
    /// Objects compare without regard to key order; everything else structurally.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Bool(a), Json::Bool(b)) => a == b,
            (Json::Number(a), Json::Number(b)) => numbers_equal(a, b),
            (Json::String(a), Json::String(b)) => a == b,
            (Json::Array(a), Json::Array(b)) => a == b,
            (Json::Object(a), Json::Object(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| other.get(k) == Some(v))
            }
            _ => false,
        }
    }
}

fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64()) {
        return x == y;
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

impl Json {
    /// An empty object.
    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    /// A string value.
    pub fn str(s: impl Into<String>) -> Json {
        Json::String(s.into())
    }

    /// An integer value.
    pub fn int(n: i64) -> Json {
        Json::Number(n.into())
    }

    /// Builds an object from pairs, in order.
    pub fn obj<const N: usize>(pairs: [(&str, Json); N]) -> Json {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Builds an array of strings.
    pub fn strings<S: AsRef<str>>(items: &[S]) -> Json {
        Json::Array(items.iter().map(|s| Json::str(s.as_ref())).collect())
    }

    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The member `key` of an object, mutably.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Sets `key` on an object: replaced in place when present, appended otherwise. False when not an object.
    pub fn insert(&mut self, key: &str, value: Json) -> bool {
        match self {
            Json::Object(m) => {
                if let Some(slot) = m.iter_mut().find(|(k, _)| k == key) {
                    slot.1 = value;
                } else {
                    m.push((key.to_string(), value));
                }
                true
            }
            _ => false,
        }
    }

    /// Removes `key` from an object, keeping the order of the rest.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Object(m) => {
                let i = m.iter().position(|(k, _)| k == key)?;
                Some(m.remove(i).1)
            }
            _ => None,
        }
    }

    /// The value at a path of object keys.
    pub fn at(&self, path: &[String]) -> Option<&Json> {
        path.iter().try_fold(self, |v, k| v.get(k))
    }

    /// The value at a path of object keys, mutably.
    pub fn at_mut(&mut self, path: &[String]) -> Option<&mut Json> {
        path.iter().try_fold(self, |v, k| v.get_mut(k))
    }

    /// The string inside, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// The elements, if this is an array.
    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The elements, if this is an array, mutably.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    /// True for an object.
    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// True for an empty object or an empty array.
    pub fn is_empty_container(&self) -> bool {
        match self {
            Json::Object(m) => m.is_empty(),
            Json::Array(a) => a.is_empty(),
            _ => false,
        }
    }

    /// True when every member of the object `self` is in `have` with an equal value (`have` may hold more).
    pub fn is_subset_of(&self, have: &Json) -> bool {
        match self {
            Json::Object(m) => have.is_object() && m.iter().all(|(k, v)| have.get(k) == Some(v)),
            other => other == have,
        }
    }

    /// Converts to a `serde_json::Value` (key order is lost there; used for the manifest).
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Json::Null => serde_json::Value::Null,
            Json::Bool(b) => serde_json::Value::Bool(*b),
            Json::Number(n) => serde_json::Value::Number(n.clone()),
            Json::String(s) => serde_json::Value::String(s.clone()),
            Json::Array(a) => serde_json::Value::Array(a.iter().map(Json::to_value).collect()),
            Json::Object(m) => serde_json::Value::Object(
                m.iter().map(|(k, v)| (k.clone(), v.to_value())).collect(),
            ),
        }
    }

    /// Converts from a `serde_json::Value`.
    pub fn from_value(v: &serde_json::Value) -> Json {
        match v {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(b) => Json::Bool(*b),
            serde_json::Value::Number(n) => Json::Number(n.clone()),
            serde_json::Value::String(s) => Json::String(s.clone()),
            serde_json::Value::Array(a) => Json::Array(a.iter().map(Json::from_value).collect()),
            serde_json::Value::Object(m) => Json::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), Json::from_value(v)))
                    .collect(),
            ),
        }
    }

    /// Compact JSON text.
    pub fn to_compact(&self) -> String {
        // Serializing this type cannot fail: every key is a string and every number is finite.
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Number(n) => n.serialize(s),
            Json::String(v) => s.serialize_str(v),
            Json::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Object(m) => {
                let mut map = s.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("number out of range"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        Json::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(Json::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        // A repeated key keeps its first position and its last value, as JavaScript's JSON.parse does.
        let mut obj = Json::object();
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            obj.insert(&k, v);
        }
        Ok(obj)
    }
}

/// How a config file parsed.
#[derive(Debug)]
pub enum Parsed {
    /// Plain JSON: safe to rewrite.
    Plain(Json),
    /// Parses only with comments or trailing commas removed: never rewritten (8.4). Holds the value for checks.
    Jsonc(Json),
    /// Doesn't parse at all.
    Invalid(String),
}

/// Parses config text, telling plain JSON from JSONC.
pub fn parse_config(text: &str) -> Parsed {
    match serde_json::from_str::<Json>(text) {
        Ok(v) => Parsed::Plain(v),
        Err(strict) => match serde_json::from_str::<Json>(&strip_jsonc(text)) {
            Ok(v) => Parsed::Jsonc(v),
            Err(_) => Parsed::Invalid(strict.to_string()),
        },
    }
}

/// Removes `//` and `/* */` comments and trailing commas outside strings.
pub fn strip_jsonc(text: &str) -> String {
    let src: Vec<char> = text.chars().collect();
    let mut no_comments = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < src.len() {
        let c = src[i];
        if in_str {
            no_comments.push(c);
            if c == '\\' && i + 1 < src.len() {
                no_comments.push(src[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match (c, src.get(i + 1)) {
            ('"', _) => {
                in_str = true;
                no_comments.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < src.len() && src[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i < src.len() && !(src[i] == '*' && src.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
                no_comments.push(' ');
            }
            _ => {
                no_comments.push(c);
                i += 1;
            }
        }
    }

    let chars: Vec<char> = no_comments.chars().collect();
    let mut out = String::with_capacity(no_comments.len());
    let mut in_str = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// The indentation and final newline of a JSON file, so a rewrite looks like the original.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    indent: String,
    trailing_newline: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            indent: "  ".to_string(),
            trailing_newline: true,
        }
    }
}

impl Style {
    /// Guesses the style of existing JSON text.
    pub fn detect(text: &str) -> Style {
        let indent = text
            .lines()
            .skip(1)
            .map(|l| &l[..l.len() - l.trim_start_matches([' ', '\t']).len()])
            .find(|ws| !ws.is_empty())
            .unwrap_or("  ")
            .to_string();
        Style {
            indent,
            trailing_newline: text.ends_with('\n'),
        }
    }

    /// Pretty-prints `v` in this style.
    pub fn render(&self, v: &Json) -> String {
        let mut buf = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
        // Writing to a Vec can't fail and Json always serializes.
        let _ = v.serialize(&mut ser);
        let mut s = String::from_utf8(buf).unwrap_or_default();
        if self.trailing_newline {
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_through_a_round_trip() {
        let text = "{\n  \"zeta\": 1,\n  \"alpha\": {\"b\": true, \"a\": null},\n  \"mid\": [1.5, \"x\"]\n}\n";
        let Parsed::Plain(v) = parse_config(text) else {
            panic!("should parse")
        };
        let style = Style::detect(text);
        assert_eq!(
            style.render(&v),
            "{\n  \"zeta\": 1,\n  \"alpha\": {\n    \"b\": true,\n    \"a\": null\n  },\n  \"mid\": [\n    1.5,\n    \"x\"\n  ]\n}\n"
        );
    }

    #[test]
    fn detects_tabs_and_missing_newline() {
        let style = Style::detect("{\n\t\"a\": 1\n}");
        assert_eq!(
            style.render(&Json::obj([("a", Json::int(1))])),
            "{\n\t\"a\": 1\n}"
        );
    }

    #[test]
    fn objects_compare_without_order() {
        let a = Json::obj([("a", Json::int(1)), ("b", Json::str("x"))]);
        let b = Json::obj([("b", Json::str("x")), ("a", Json::int(1))]);
        assert_eq!(a, b);
        assert_ne!(a, Json::obj([("a", Json::int(1))]));
        assert!(Json::obj([("a", Json::int(1))]).is_subset_of(&a));
        assert!(!a.is_subset_of(&Json::obj([("a", Json::int(1))])));
    }

    #[test]
    fn tells_jsonc_from_json_and_garbage() {
        assert!(matches!(parse_config("{\"a\": 1}"), Parsed::Plain(_)));
        let jsonc = "{\n  // a comment\n  \"a\": \"http://x\", /* block */\n  \"b\": [1, 2,],\n}\n";
        match parse_config(jsonc) {
            Parsed::Jsonc(v) => {
                assert_eq!(v.get("a").and_then(Json::as_str), Some("http://x"));
                assert_eq!(
                    v.get("b"),
                    Some(&Json::Array(vec![Json::int(1), Json::int(2)]))
                );
            }
            other => panic!("expected JSONC, got {other:?}"),
        }
        assert!(matches!(parse_config("{\"a\": "), Parsed::Invalid(_)));
        assert!(matches!(
            parse_config("{\"a\": \"// not a comment, \"}"),
            Parsed::Plain(_)
        ));
    }

    #[test]
    fn repeated_keys_keep_the_last_value() {
        let Parsed::Plain(v) = parse_config("{\"a\": 1, \"b\": 2, \"a\": 3}") else {
            panic!()
        };
        assert_eq!(v.to_compact(), "{\"a\":3,\"b\":2}");
    }
}
