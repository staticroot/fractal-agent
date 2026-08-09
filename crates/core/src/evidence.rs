//! Evidence is a read-time view, not a stored object: what recomputes to the
//! same answer is derived on demand, and only what witnessed a moment is stored.
//!
//! There is deliberately no consent record. Consent happens in the principal's
//! session, at a prompt the agent can neither see nor launch, so writing down
//! "a human approved this" would store a claim and call it a fact. The signature
//! over a device-issued nonce is the consent, and the verifying key names the
//! authority that gave it.

use serde::{Deserialize, Serialize};

use crate::diff::SemanticDiff;
use crate::generations::Generation;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Witnessed, as recorded at the moment of activation.
    pub generation: Generation,
    /// Derived now, against the generation this one descended from. `None` for
    /// the first, which descended from nothing.
    pub change: Option<SemanticDiff>,
}
