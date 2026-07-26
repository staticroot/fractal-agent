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
