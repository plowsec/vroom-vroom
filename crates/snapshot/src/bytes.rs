//! `#[serde(with = "vroom_snapshot::bytes")]` for byte fields: base64 in
//! the JSON instead of an array of numbers (four times smaller, and
//! nesting no longer multiplies it). Arrays from older snapshots still read.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(b))
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("base64 or an array of bytes")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
            STANDARD.decode(v).map_err(E::custom)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 20));
            while let Some(b) = seq.next_element()? {
                out.push(b);
            }
            Ok(out)
        }
    }
    d.deserialize_any(V)
}

/// A byte vector that serializes as base64 (for maps and tuples).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Bytes(#[serde(with = "self")] pub Vec<u8>);

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct T {
        #[serde(with = "super")]
        b: Vec<u8>,
    }

    #[test]
    fn base64_and_old_arrays() {
        let t = T { b: vec![0, 1, 2, 250, 255] };
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(j, r#"{"b":"AAEC+v8="}"#);
        assert_eq!(serde_json::from_str::<T>(&j).unwrap(), t);
        assert_eq!(serde_json::from_str::<T>(r#"{"b":[0,1,2,250,255]}"#).unwrap(), t);
        assert!(serde_json::from_str::<T>(r#"{"b":"!!"}"#).is_err());
        let m: std::collections::BTreeMap<String, Bytes> = serde_json::from_str(r#"{"x":"AQI="}"#).unwrap();
        assert_eq!(m["x"].0, [1, 2]);
    }
}
