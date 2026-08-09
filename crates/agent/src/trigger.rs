//! The agent's side of the trigger contract. The trigger is the reference
//! monitor: it mints the nonce, verifies the signature the principal obtained,
//! burns it, and switches. The agent only relays: it holds no key and decides
//! nothing about authority.

/// Mirrors `systems.staticroot.Trigger`. `IssueNonce` and `SwitchToStorePath`
/// are the two the agent calls; `Progress` is the signal it forwards to the
/// principal while a switch runs. `LockScreen` is deliberately absent: the
/// standalone agent never locks, and has no key that could sign for it.
#[zbus::proxy(
    interface = "systems.staticroot.Trigger",
    default_service = "systems.staticroot.Trigger",
    default_path = "/systems/staticroot/Trigger"
)]
pub trait Trigger {
    fn issue_nonce(&self) -> zbus::Result<String>;

    /// Returns the hex-encoded public key that verified the signature. That key
    /// is the authority behind the generation, witnessed by the trigger rather
    /// than asserted by the caller.
    fn switch_to_store_path(
        &self,
        store_path: &str,
        signature: &str,
        nonce: &str,
    ) -> zbus::Result<String>;

    #[zbus(signal)]
    fn progress(&self, line: String) -> zbus::Result<()>;
}
