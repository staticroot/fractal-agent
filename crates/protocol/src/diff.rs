use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChange {
    pub key: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub options: Vec<OptionChange>,
    pub closure: ClosureDiff,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClosureDiff {
    pub packages: BTreeMap<String, PackageDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageDelta {
    pub size_delta: i64,
    pub versions_before: Vec<String>,
    pub versions_after: Vec<String>,
}
