//! How Storage's state is written to and read from a snapshot, where its
//! types are not plain data: content as base64 rather than a list of
//! numbers, and the `&'static str` names some types keep.

use axum::body::Bytes;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A name read back from a snapshot. Names come from a small fixed set, so
/// the little this leaks once per load is bounded.
fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

pub(crate) mod bytes {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(data: &Bytes, serializer: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(data).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Bytes, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map(Bytes::from).map_err(D::Error::custom)
    }
}

/// Staged blocks: blob name → `(block id, data)`, in order.
pub(crate) mod staged_blocks {
    use std::collections::BTreeMap;

    use super::*;

    type Staged = BTreeMap<String, Vec<(String, Bytes)>>;

    pub(crate) fn serialize<S: Serializer>(blocks: &Staged, serializer: S) -> Result<S::Ok, S::Error> {
        let encoded: BTreeMap<&String, Vec<(&String, String)>> =
            blocks.iter().map(|(blob, list)| (blob, list.iter().map(|(id, data)| (id, STANDARD.encode(data))).collect())).collect();
        encoded.serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Staged, D::Error> {
        let encoded = BTreeMap::<String, Vec<(String, String)>>::deserialize(deserializer)?;
        encoded
            .into_iter()
            .map(|(blob, list)| {
                let list = list.into_iter().map(|(id, data)| STANDARD.decode(data).map(|d| (id, Bytes::from(d)))).collect::<Result<_, _>>();
                list.map(|l| (blob, l)).map_err(D::Error::custom)
            })
            .collect()
    }
}

/// `(header name, value)` pairs whose names are `&'static str`.
pub(crate) mod named_values {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(pairs: &[(&'static str, String)], serializer: S) -> Result<S::Ok, S::Error> {
        pairs.serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<(&'static str, String)>, D::Error> {
        let pairs = Vec::<(String, String)>::deserialize(deserializer)?;
        Ok(pairs.into_iter().map(|(name, value)| (leak(name), value)).collect())
    }
}

/// An immutability policy's history: `(update, days, timestamp)`.
pub(crate) mod history {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(entries: &[(&'static str, i64, String)], serializer: S) -> Result<S::Ok, S::Error> {
        entries.serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<(&'static str, i64, String)>, D::Error> {
        let entries = Vec::<(String, i64, String)>::deserialize(deserializer)?;
        Ok(entries.into_iter().map(|(update, days, at)| (leak(update), days, at)).collect())
    }
}
