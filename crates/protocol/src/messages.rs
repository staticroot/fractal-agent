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
    Candidate { commit: String },
    Running,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftChange {
    #[serde(flatten)]
    pub change: OptionChange,
    pub author: Option<u32>,
}

/// A draft the running configuration has made unapplicable, left on the commit
/// it was drafted against until its author edits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuarantinedDraft {
    pub author: u32,
    /// Option keys and file paths mixed, because both quarantine and both are
    /// only ever displayed.
    pub conflicts: Vec<String>,
}

/// Which commit a file read resolves against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Revision {
    /// A reference or an object id, and `None` for the branch tip.
    Commit {
        #[serde(default)]
        commit: Option<String>,
    },
    /// `None` is the caller's own. Naming another principal is allowed, since
    /// reads need no authorization.
    Draft {
        #[serde(default)]
        author: Option<u32>,
    },
    Generation {
        id: i64,
    },
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
    /// Drafts a value against the calling principal. Another principal's draft
    /// of the same option is neither displaced nor in the way.
    SetOption { key: String, value: Value },
    UnsetOption { key: String },
    Drafts,
    /// Discards the caller's own draft, all of it or the keys named.
    Discard {
        #[serde(default)]
        keys: Vec<String>,
    },
    /// Builds the caller's own draft as a candidate commit that becomes history
    /// only if it is activated.
    Build { message: Option<String> },
    ListFiles { at: Revision },
    ReadFile { at: Revision, path: String },
    /// Lands a whole file in the caller's draft. `base_digest` identifies the
    /// version the session read, and a mismatch is refused rather than merged.
    WriteFile {
        path: String,
        contents: String,
        base_digest: String,
    },
    Diff { from: Endpoint, to: Endpoint },
    Evidence { generation: i64 },
    BeginActivation { commit: String },
    CompleteActivation {
        commit: String,
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
    Drafts {
        changes: Vec<DraftChange>,
        quarantined: Vec<QuarantinedDraft>,
    },
    Files { paths: Vec<String> },
    FileContents { contents: String, digest: String },
    Built { store_path: String, commit: String },
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

    #[test]
    fn a_draft_revision_without_an_author_is_the_callers_own() {
        let json = r#"{"at":"draft"}"#;
        assert_eq!(
            serde_json::from_str::<Revision>(json).unwrap(),
            Revision::Draft { author: None }
        );
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
