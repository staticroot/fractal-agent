use serde::{Deserialize, Serialize};

use crate::diff::SemanticDiff;
use crate::generations::Generation;

/// There is no consent record on purpose. Consent happens at a prompt in the
/// principal's session that the agent can neither see nor launch, so a stored
/// "a human approved this" would be a claim written down as a fact. The signature
/// over the nonce is the consent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub generation: Generation,
    pub change: Option<SemanticDiff>,
}
