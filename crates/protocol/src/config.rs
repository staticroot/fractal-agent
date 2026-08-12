use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `untagged` so it round-trips with the JSON `nix eval` emits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Attrs(BTreeMap<String, Value>),
}
