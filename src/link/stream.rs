//! Per-stream helpers: a call's head, read before its body (5.5 step 2).

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::wire::CallKind;

/// What a call is, read without parsing its body: the kind, and the `id` a rate-limited `deliver` is refused
/// with (5.5 step 2). Every other field is skipped without being built.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CallHead {
    /// The call `t` names, if it is a string naming one.
    pub(crate) kind: Option<CallKind>,
    /// The top-level `id`, if it is a string.
    pub(crate) id: Option<String>,
}

impl CallHead {
    /// Reads the head of a request payload; an empty head when it isn't a JSON object. A repeated key resolves
    /// as in [`crate::wire::parse_incoming`], the last one winning, so the head names the call that parse would.
    pub(crate) fn read(payload: &[u8]) -> CallHead {
        serde_json::from_slice(payload).unwrap_or_default()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Field {
    T,
    Id,
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for CallHead {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HeadVisitor;

        impl<'de> Visitor<'de> for HeadVisitor {
            type Value = CallHead;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a call object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CallHead, A::Error> {
                let (mut t, mut id) = (None, None);
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::T => t = map.next_value::<MaybeStr>()?.0,
                        Field::Id => id = map.next_value::<MaybeStr>()?.0,
                        Field::Other => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(CallHead {
                    kind: t.as_deref().and_then(CallKind::from_name),
                    id,
                })
            }
        }

        deserializer.deserialize_map(HeadVisitor)
    }
}

/// A JSON string, or `None` for any other value, which is skipped without being built.
struct MaybeStr(Option<String>);

impl<'de> Deserialize<'de> for MaybeStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MaybeStrVisitor;

        impl<'de> Visitor<'de> for MaybeStrVisitor {
            type Value = MaybeStr;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<MaybeStr, E> {
                Ok(MaybeStr(Some(v.to_owned())))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<MaybeStr, E> {
                Ok(MaybeStr(None))
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<MaybeStr, E> {
                Ok(MaybeStr(None))
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<MaybeStr, E> {
                Ok(MaybeStr(None))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<MaybeStr, E> {
                Ok(MaybeStr(None))
            }

            fn visit_unit<E: de::Error>(self) -> Result<MaybeStr, E> {
                Ok(MaybeStr(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<MaybeStr, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MaybeStr(None))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<MaybeStr, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(MaybeStr(None))
            }
        }

        deserializer.deserialize_any(MaybeStrVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::parse_incoming;

    fn head(json: &str) -> CallHead {
        CallHead::read(json.as_bytes())
    }

    #[test]
    fn the_head_reads_t_and_id_only() {
        let id = "01JA0000000000000000000000";
        assert_eq!(
            head(&format!(
                r#"{{"t":"deliver","id":"{id}","body":{{"deep":[1,2,{{"x":null}}]}},"n":1.5}}"#
            )),
            CallHead {
                kind: Some(CallKind::Deliver),
                id: Some(id.into()),
            }
        );
        assert_eq!(head(r#"{"t":"presence"}"#).kind, Some(CallKind::Presence));
        assert_eq!(head(r#"{"t":"teleport"}"#), CallHead::default());
        assert_eq!(head(r#"{"t":7,"id":["x"]}"#), CallHead::default());
        assert_eq!(head(r#"{"t":"ping"}"#).kind, Some(CallKind::Ping));
        for not_a_call in ["", "[]", "\"deliver\"", "{\"t\":\"ping\"", "{} trailing"] {
            assert_eq!(head(not_a_call), CallHead::default(), "{not_a_call}");
        }
    }

    #[test]
    fn the_head_names_the_call_the_full_parse_would() {
        let frames = [
            r#"{"t":"ping"}"#,
            r#"{"t":"ping","t":"deliver","id":"x"}"#,
            r#"{"t":"deliver","t":"presence"}"#,
            r#"{"t":"deliver","t":5}"#,
            r#"{"t":"deliver","x":{"t":"ping"}}"#,
            r#"{"x":{"t":"deliver"},"t":"presence"}"#,
        ];
        for frame in frames {
            let parsed = parse_incoming(frame.as_bytes())
                .ok()
                .map(|call| call.kind());
            assert_eq!(head(frame).kind, parsed, "{frame}");
        }
        assert_eq!(
            head(r#"{"t":"deliver","id":"a","id":"b"}"#).id.as_deref(),
            Some("b"),
            "the last id wins"
        );
    }
}
