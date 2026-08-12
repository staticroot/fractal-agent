use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogEntry, OptionRead};
use crate::config::Value;
use crate::diff::{OptionChange, SemanticDiff};
use crate::evidence::Evidence;
use crate::generations::Generation;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    Activation { store_path: String, nonce: String },
    /// Managed mode only. The standalone lawyer refuses to sign this one.
    Lock { nonce: String },
}

impl Payload {
    pub fn kind(&self) -> &'static str {
        match self {
            Payload::Activation { .. } => "activation",
            Payload::Lock { .. } => "lock",
        }
    }
}

/// A seam for managed mode, which adds its own authorities here. `LocalKey` is the
/// only one v0 wires up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    LocalKey,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub method: Method,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ref", rename_all = "snake_case")]
pub enum Endpoint {
    Generation { id: i64 },
    Build { store_path: String },
    Running,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedChange {
    #[serde(flatten)]
    pub change: OptionChange,
    pub staged_by: Option<u32>,
}

pub fn fingerprint(changes: &[StagedChange]) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for staged in changes {
        staged.change.key.hash(&mut hasher);
        staged.staged_by.hash(&mut hasher);
        // JSON rather than the value itself, which holds floats and so is not Hash.
        serde_json::to_string(&staged.change.after)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Solution {
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    History,
    Current,
    Catalog,
    GetOption { key: String },
    SetOption {
        key: String,
        value: Value,
        #[serde(default)]
        override_staged: bool,
    },
    UnsetOption {
        key: String,
        #[serde(default)]
        override_staged: bool,
    },
    StagedDiff,
    Commit {
        message: Option<String>,
        /// The [`fingerprint`] the caller last saw. Omitting it is refused unless
        /// everything staged is the caller's own.
        #[serde(default)]
        expect: Option<String>,
    },
    Discard {
        #[serde(default)]
        all: bool,
    },
    Build,
    Diff { from: Endpoint, to: Endpoint },
    Evidence { generation: i64 },
    BeginActivation { store_path: String },
    CompleteActivation {
        store_path: String,
        nonce: String,
        solution: Solution,
    },
    /// Names a generation rather than a store path. A forward edit can land on a
    /// closure that already appears in history, so a path cannot say what was meant.
    BeginRollback { generation: i64 },
    CompleteRollback {
        generation: i64,
        nonce: String,
        solution: Solution,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Generations { generations: Vec<Generation> },
    Current { generation: Option<Box<Generation>> },
    Catalog { entries: Vec<CatalogEntry> },
    OptionValue(Box<OptionRead>),
    StagedDiff { changes: Vec<StagedChange>, fingerprint: String },
    Committed { commit: Option<String> },
    Built { store_path: String, config_commit: String },
    Diff(Box<SemanticDiff>),
    Evidence(Box<Evidence>),
    Activated { generation: Box<Generation> },
    Challenge(Challenge),
    Progress { line: String },
    Ok,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_kind_matches_variant() {
        let act = Payload::Activation {
            store_path: "/nix/store/x".into(),
            nonce: "n".into(),
        };
        assert_eq!(act.kind(), "activation");
        assert_eq!(Payload::Lock { nonce: "n".into() }.kind(), "lock");
    }

    fn staged(key: &str, uid: u32, after: i64) -> StagedChange {
        StagedChange {
            change: OptionChange {
                key: key.into(),
                before: None,
                after: Some(crate::config::Value::Int(after)),
            },
            staged_by: Some(uid),
        }
    }

    #[test]
    fn the_fingerprint_moves_with_every_part_of_a_staged_change() {
        let base = vec![staged("a", 1000, 1)];
        assert_eq!(fingerprint(&base), fingerprint(&base.clone()));

        assert_ne!(fingerprint(&base), fingerprint(&[staged("b", 1000, 1)]), "key");
        assert_ne!(fingerprint(&base), fingerprint(&[staged("a", 1001, 1)]), "author");
        assert_ne!(fingerprint(&base), fingerprint(&[staged("a", 1000, 2)]), "value");
        assert_ne!(
            fingerprint(&base),
            fingerprint(&[staged("a", 1000, 1), staged("b", 1001, 1)]),
            "an addition by somebody else"
        );
        assert_ne!(fingerprint(&base), fingerprint(&[]), "everything discarded");
    }

    #[test]
    fn challenge_round_trips_as_tagged_json() {
        let c = Challenge {
            method: Method::LocalKey,
            payload: Payload::Activation {
                store_path: "/nix/store/x".into(),
                nonce: "deadbeef".into(),
            },
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""method":"local_key""#), "got: {json}");
        assert!(json.contains(r#""kind":"activation""#), "got: {json}");
        assert_eq!(serde_json::from_str::<Challenge>(&json).unwrap(), c);
    }
}
