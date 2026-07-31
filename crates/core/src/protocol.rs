//! The activation handshake, as data. A principal (the CLI, a GUI, later a
//! management console) asks the agent to activate; the agent mints a nonce from
//! the trigger and hands back a [`Challenge`]; the principal solves it out of
//! band — for the standalone key, by invoking the lawyer through pkexec, which
//! is where the human consents — and returns a [`Solution`]; the agent relays
//! that to the trigger and records the outcome.
//!
//! Everything here is typed. The principal and the agent only ever see
//! [`Payload`] fields, never the bytes that get signed: only the lawyer (to
//! sign) and the trigger (to verify) know the byte encoding. That keeps honest
//! consent renderable and stops any party from turning the lawyer into a raw
//! signing oracle.

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogEntry;
use crate::config::Value;
use crate::diff::OptionChange;
use crate::generations::Generation;

/// What a signature authorizes, in typed form. The lawyer and the trigger each
/// reconstruct the exact bytes from these fields; the variant name is also the
/// lawyer's `--kind`. `Lock` has no store path and is the managed-mode seam: the
/// standalone lawyer does not sign it (the cloud does).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    Activation { store_path: String, nonce: String },
    Lock { nonce: String },
}

impl Payload {
    /// The lawyer `--kind` this payload maps to.
    pub fn kind(&self) -> &'static str {
        match self {
            Payload::Activation { .. } => "activation",
            Payload::Lock { .. } => "lock",
        }
    }
}

/// Which authority produces the proof for a challenge. Only [`Method::LocalKey`]
/// is wired in v0; the rest are managed/future seams. Left a single method for
/// now; a challenge may later carry several to compose (e.g. sign + quorum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// The root-held standalone key, signed by the lawyer under human consent.
    LocalKey,
}

/// The agent's request for a proof over `payload`, to be produced by `method`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub method: Method,
    pub payload: Payload,
}

/// The principal's answer to a challenge: the proof over the payload. For an
/// Ed25519 signature this is the hex-encoded 64-byte signature. The agent treats
/// it as untrusted bytes and lets the trigger re-verify.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Solution {
    pub signature: String,
}

/// One request from a principal over the socket, one JSON object per line. The
/// principal holds the in-flight activation context, so `complete_activation`
/// carries back everything the agent needs to switch; it keeps no pending
/// state between the two calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Every recorded generation, oldest first.
    History,
    /// The generation running now (latest successful activation).
    Current,
    /// The curated set of options a principal may read and change.
    Catalog,
    /// The staged value of one option, or `None` if it is not set.
    GetOption { key: String },
    /// Stage a value for one option. Rejected if the key is not in the catalog
    /// or the value falls outside its allowed set.
    SetOption { key: String, value: Value },
    /// Stage the removal of one option.
    UnsetOption { key: String },
    /// The option-level changes staged since the last apply.
    StagedDiff,
    /// Commit the staged change to the config repository.
    Apply { message: Option<String> },
    /// Discard the staged change, restoring the committed configuration.
    Discard,
    /// Mint a nonce and return the [`Challenge`] to sign for activating
    /// `store_path`.
    BeginActivation { store_path: String },
    /// Hand back a solved challenge; the agent relays it to the trigger and
    /// records the outcome.
    CompleteActivation {
        store_path: String,
        nonce: String,
        solution: Solution,
    },
}

/// One line of the agent's answer. A streaming operation emits any number of
/// `Progress` lines before exactly one terminal line (`Ok`, `Error`, or a
/// result variant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Generations { generations: Vec<Generation> },
    Current { generation: Option<Box<Generation>> },
    Catalog { entries: Vec<CatalogEntry> },
    OptionValue { key: String, value: Option<Value> },
    StagedDiff { changes: Vec<OptionChange> },
    /// Terminal success of an apply; `commit` is the new commit hash, or `None`
    /// when there was nothing staged to commit.
    Applied { commit: Option<String> },
    Challenge(Challenge),
    /// A build or activation log line, forwarded as it happens.
    Progress { line: String },
    /// Terminal success with no payload.
    Ok,
    /// Terminal failure.
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
