//! The message the signature authorizes. This is a byte-for-byte mirror of the
//! trigger's `encoding.rs`; the known-answer tests below carry the identical
//! frozen vectors, so if either side ever changes the layout the KATs break
//! before anything ships.

pub const CONTEXT: &[u8] = b"systems.staticroot.trigger/activation/v1";

/// `CONTEXT ‖ len(store) ‖ store ‖ len(nonce) ‖ nonce`, each length a
/// little-endian `u64`.
pub fn activation_message(store_path: &str, nonce: &str) -> Vec<u8> {
    let store = store_path.as_bytes();
    let nonce = nonce.as_bytes();
    let mut msg = Vec::with_capacity(CONTEXT.len() + 16 + store.len() + nonce.len());
    msg.extend_from_slice(CONTEXT);
    msg.extend_from_slice(&(store.len() as u64).to_le_bytes());
    msg.extend_from_slice(store);
    msg.extend_from_slice(&(nonce.len() as u64).to_le_bytes());
    msg.extend_from_slice(nonce);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // Frozen vectors — identical to fractal-trigger's, locking the two together.
    const KAT_SEED: [u8; 32] = [7u8; 32];
    const KAT_STORE: &str = "/nix/store/00000000000000000000000000000000-x";
    const KAT_NONCE: &str = "deadbeef";
    const KAT_MESSAGE_HEX: &str = "73797374656d732e737461746963726f6f742e747269676765722f61637469766174696f6e2f76312d000000000000002f6e69782f73746f72652f30303030303030303030303030303030303030303030303030303030303030302d7808000000000000006465616462656566";
    const KAT_SIGNATURE_HEX: &str = "eb0cf6e0622b2d460f741d222b04715329f773c585d47eb493955e9eaf98ac0ef274653dc16c7e025d3f67b197f2fe8319d89fa34707a1e558a80a0f13eead06";

    #[test]
    fn message_kat() {
        assert_eq!(hex::encode(activation_message(KAT_STORE, KAT_NONCE)), KAT_MESSAGE_HEX);
    }

    #[test]
    fn signature_kat() {
        let sk = SigningKey::from_bytes(&KAT_SEED);
        let sig = sk.sign(&activation_message(KAT_STORE, KAT_NONCE));
        assert_eq!(hex::encode(sig.to_bytes()), KAT_SIGNATURE_HEX);
    }
}
