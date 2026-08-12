use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Forward,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    pub path: String,
    pub size: u64,
    pub tail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub id: i64,
    pub timestamp: Timestamp,
    pub store_path: String,
    pub config_commit: String,
    pub parent_id: Option<i64>,
    pub kind: Kind,
    pub description: String,
    pub actor: String,
    pub verifying_key: String,
    pub signature: String,
    pub burned_nonce: String,
    pub outcome: Outcome,
    pub policy_version: Option<String>,
    pub build_log: Option<LogRef>,
    pub activation_log: Option<LogRef>,
}
