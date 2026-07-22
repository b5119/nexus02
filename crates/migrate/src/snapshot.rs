use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Backward-compatible serde module for `Vec<u8>`: serializes as base64,
/// deserializes both base64 (new) and JSON array-of-numbers (legacy).
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(bytes).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        use serde::de;

        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a base64 string or a JSON array of numbers")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
                STANDARD.decode(v).map_err(de::Error::custom)
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut bytes = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    bytes.push(b);
                }
                Ok(bytes)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    LastWriteWins,
    AppMerge,
    KeepBoth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEntry {
    #[serde(with = "base64_bytes")]
    pub value: Vec<u8>,
    pub conflict_policy: ConflictPolicy,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub device_id: String,
    pub vector_clock: std::collections::BTreeMap<String, u64>,
    pub keys: HashMap<String, StateEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConflictEntry {
    pub key: String,
    #[serde(with = "base64_bytes")]
    pub local_value: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub remote_value: Vec<u8>,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ConflictSet {
    pub conflicts: Vec<ConflictEntry>,
    pub schema_dropped_keys: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let entry = StateEntry {
            value: b"hello world".to_vec(),
            conflict_policy: ConflictPolicy::AppMerge,
            schema_version: 1,
        };
        let json = serde_json::to_string(&entry).unwrap();
        // Should be base64-encoded, not a JSON array of numbers.
        assert!(
            json.contains("aGVsbG8gd29ybGQ="),
            "expected base64 in {json}"
        );
        let decoded: StateEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.value, b"hello world");
    }

    #[test]
    fn base64_backward_compat_legacy_array() {
        // Simulate the old serde_json array-of-numbers format.
        let legacy_json = r#"{"value":[72,101,108,108,111],"conflict_policy":"LastWriteWins","schema_version":1}"#;
        let entry: StateEntry = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(entry.value, b"Hello");
    }

    #[test]
    fn base64_empty_value() {
        let entry = StateEntry {
            value: vec![],
            conflict_policy: ConflictPolicy::KeepBoth,
            schema_version: 2,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: StateEntry = serde_json::from_str(&json).unwrap();
        assert!(decoded.value.is_empty());
    }

    #[test]
    fn snapshot_round_trip_uses_base64() {
        let snapshot = AppSnapshot {
            device_id: "test".into(),
            vector_clock: std::collections::BTreeMap::from([("t".into(), 1)]),
            keys: HashMap::from([(
                "k".into(),
                StateEntry {
                    value: b"\xff\xfe".to_vec(),
                    conflict_policy: ConflictPolicy::AppMerge,
                    schema_version: 1,
                },
            )]),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        // Non-UTF8 bytes should be base64, not an array.
        assert!(
            json.contains("//4="),
            "expected base64 for binary data in {json}"
        );
        let decoded: AppSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.keys.get("k").unwrap().value, b"\xff\xfe");
    }
}
